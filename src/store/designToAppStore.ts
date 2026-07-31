import { create } from 'zustand';

import * as api from '../lib/designToApp';
import type {
  DesignBrowserEvidence,
  DesignImplementationPlan,
  DesignPatchSummary,
  DesignSource,
  DesignVerificationResult,
  DesignWorktree,
} from '../lib/designToApp';
import { useVerifyStore } from './verifyStore';
import { errorMessage } from "../lib/errors";
import { hydrateState, persistState } from "../lib/persistedState";

export const DESIGN_TO_APP_STORAGE_KEY = 'little-monkey-design-to-app-projects-v1';
const STORAGE_VERSION = 1;

export type DesignToAppStatus =
  | 'draft'
  | 'planning'
  | 'planned'
  | 'capturing_before'
  | 'creating_worktree'
  | 'implementing'
  | 'capturing_after'
  | 'completed'
  | 'failed'
  | 'cancelled';

export interface DesignToAppProject {
  id: string;
  title: string;
  description: string;
  repositorySlug: string;
  previewUrl: string;
  sources: DesignSource[];
  verificationCommandIds: string[];
  status: DesignToAppStatus;
  plan: DesignImplementationPlan | null;
  worktree: DesignWorktree | null;
  patch: DesignPatchSummary | null;
  verification: DesignVerificationResult[];
  beforeEvidence: DesignBrowserEvidence | null;
  afterEvidence: DesignBrowserEvidence | null;
  implementationSummary: string | null;
  error: string | null;
  createdAtMs: number;
  updatedAtMs: number;
}

type EditableProjectFields = Pick<
  DesignToAppProject,
  'title' | 'description' | 'repositorySlug' | 'previewUrl' | 'verificationCommandIds'
>;

interface DesignToAppState {
  projects: DesignToAppProject[];
  selectedProjectId: string | null;
  activityByProject: Record<string, string>;
  init: () => void;
  selectProject: (projectId: string | null) => void;
  createProject: (input: { title: string; description?: string; repositorySlug?: string }) => DesignToAppProject;
  updateProject: (projectId: string, patch: Partial<EditableProjectFields>) => void;
  addSource: (projectId: string, source: DesignSource) => void;
  replaceSource: (projectId: string, sourceId: string, source: DesignSource) => void;
  removeSource: (projectId: string, sourceId: string) => void;
  analyze: (projectId: string) => Promise<void>;
  run: (projectId: string) => Promise<void>;
  captureEvidence: (projectId: string, phase: 'before' | 'after') => Promise<void>;
  cancel: (projectId: string) => void;
  clearError: (projectId: string) => void;
  deleteProject: (projectId: string) => void;
}

const STATUSES: ReadonlySet<DesignToAppStatus> = new Set([
  'draft',
  'planning',
  'planned',
  'capturing_before',
  'creating_worktree',
  'implementing',
  'capturing_after',
  'completed',
  'failed',
  'cancelled',
]);

const RUNNING_STATUSES: ReadonlySet<DesignToAppStatus> = new Set([
  'planning',
  'capturing_before',
  'creating_worktree',
  'implementing',
  'capturing_after',
]);

const controllers = new Map<string, AbortController>();

export function __resetDesignToAppControllersForTests(): void {
  controllers.clear();
}

function errorText(error: unknown): string {
  return errorMessage(error);
}

function portableProjects(projects: readonly DesignToAppProject[]): DesignToAppProject[] {
  return projects.map((project) => ({
    ...project,
    sources: project.sources.map((source) => ({ ...source, imageDataUrl: null })),
  }));
}

function persist(projects: readonly DesignToAppProject[]): void {
  persistState(DESIGN_TO_APP_STORAGE_KEY, STORAGE_VERSION, { projects: portableProjects(projects) });
}

function isPlan(value: unknown): value is DesignImplementationPlan {
  if (!value || typeof value !== 'object') return false;
  const item = value as Partial<DesignImplementationPlan>;
  return typeof item.planId === 'string'
    && typeof item.sourceRevision === 'string'
    && typeof item.summary === 'string'
    && Array.isArray(item.routes)
    && Array.isArray(item.components)
    && Array.isArray(item.tokens)
    && Array.isArray(item.steps)
    && Array.isArray(item.accessibilityChecklist)
    && Array.isArray(item.verificationHints)
    && typeof item.generatedAtMs === 'number';
}

function isWorktree(value: unknown): value is DesignWorktree {
  if (!value || typeof value !== 'object') return false;
  const item = value as Partial<DesignWorktree>;
  return typeof item.worktreeId === 'string'
    && typeof item.branch === 'string'
    && typeof item.workspaceLabel === 'string'
    && typeof item.canonicalPath === 'string';
}

function hydrate(): DesignToAppProject[] {
  const raw = hydrateState(DESIGN_TO_APP_STORAGE_KEY, STORAGE_VERSION);
  if (!raw || !Array.isArray(raw.projects)) return [];
  return raw.projects.flatMap((value): DesignToAppProject[] => {
    if (!value || typeof value !== 'object') return [];
    const item = value as Partial<DesignToAppProject>;
    if (
      typeof item.id !== 'string'
      || typeof item.title !== 'string'
      || typeof item.createdAtMs !== 'number'
      || typeof item.updatedAtMs !== 'number'
      || !STATUSES.has(item.status as DesignToAppStatus)
    ) return [];
    const sources = Array.isArray(item.sources)
      ? item.sources.map(api.hydrateDesignSource).filter((source): source is DesignSource => source !== null)
      : [];
    const interrupted = RUNNING_STATUSES.has(item.status as DesignToAppStatus);
    const plan = isPlan(item.plan) ? item.plan : null;
    return [{
      id: item.id,
      title: item.title.slice(0, 300),
      description: typeof item.description === 'string' ? item.description.slice(0, 8_000) : '',
      repositorySlug: typeof item.repositorySlug === 'string' ? item.repositorySlug.slice(0, 300) : '',
      previewUrl: typeof item.previewUrl === 'string' ? item.previewUrl.slice(0, 2_000) : '',
      sources,
      verificationCommandIds: Array.isArray(item.verificationCommandIds)
        ? item.verificationCommandIds.filter((id): id is string => typeof id === 'string').slice(0, 12)
        : [],
      status: interrupted ? (plan ? 'planned' : 'draft') : item.status as DesignToAppStatus,
      plan,
      worktree: isWorktree(item.worktree) ? item.worktree : null,
      patch: item.patch && typeof item.patch === 'object' ? item.patch as DesignPatchSummary : null,
      verification: Array.isArray(item.verification)
        ? item.verification.filter((result): result is DesignVerificationResult => Boolean(
            result && typeof result === 'object' && typeof (result as DesignVerificationResult).commandId === 'string',
          )).slice(0, 12)
        : [],
      beforeEvidence: item.beforeEvidence && typeof item.beforeEvidence === 'object'
        ? item.beforeEvidence as DesignBrowserEvidence
        : null,
      afterEvidence: item.afterEvidence && typeof item.afterEvidence === 'object'
        ? item.afterEvidence as DesignBrowserEvidence
        : null,
      implementationSummary: typeof item.implementationSummary === 'string' ? item.implementationSummary : null,
      error: interrupted
        ? 'The previous local run was interrupted when the app closed. Review saved history and retry; image inputs must be re-imported.'
        : typeof item.error === 'string' ? item.error : null,
      createdAtMs: item.createdAtMs,
      updatedAtMs: item.updatedAtMs,
    }];
  });
}

function upsert(projects: DesignToAppProject[], project: DesignToAppProject): DesignToAppProject[] {
  const index = projects.findIndex((candidate) => candidate.id === project.id);
  if (index < 0) return [project, ...projects];
  const copy = [...projects];
  copy[index] = project;
  return copy;
}

export const useDesignToAppStore = create<DesignToAppState>((set, get) => {
  const update = (projectId: string, patch: Partial<DesignToAppProject>): DesignToAppProject | null => {
    let updated: DesignToAppProject | null = null;
    set((state) => {
      const current = state.projects.find((project) => project.id === projectId);
      if (!current) return state;
      updated = { ...current, ...patch, updatedAtMs: Date.now() };
      const projects = upsert(state.projects, updated);
      persist(projects);
      return { projects };
    });
    return updated;
  };

  const setActivity = (projectId: string, activity: string | null): void => {
    set((state) => {
      const activityByProject = { ...state.activityByProject };
      if (activity) activityByProject[projectId] = activity;
      else delete activityByProject[projectId];
      return { activityByProject };
    });
  };

  const resetGenerated = (projectId: string, patch: Partial<DesignToAppProject> = {}): void => {
    update(projectId, {
      status: 'draft',
      plan: null,
      patch: null,
      verification: [],
      beforeEvidence: null,
      afterEvidence: null,
      implementationSummary: null,
      error: null,
      ...patch,
    });
  };

  const capture = async (
    project: DesignToAppProject,
    phase: 'before' | 'after',
    signal?: AbortSignal,
  ): Promise<DesignBrowserEvidence> => {
    setActivity(project.id, `browser:${phase}`);
    const result = await api.captureDesignBrowserEvidence({
      projectId: project.id,
      phase,
      url: project.previewUrl,
      signal,
    });
    update(project.id, phase === 'before' ? { beforeEvidence: result } : { afterEvidence: result });
    return result;
  };

  return {
    projects: hydrate(),
    selectedProjectId: null,
    activityByProject: {},

    init: () => {
      const projects = hydrate();
      persist(projects);
      set((state) => ({
        projects,
        selectedProjectId: state.selectedProjectId && projects.some((item) => item.id === state.selectedProjectId)
          ? state.selectedProjectId
          : projects[0]?.id ?? null,
      }));
    },

    selectProject: (selectedProjectId) => set({ selectedProjectId }),

    createProject: (input) => {
      const title = input.title.trim();
      if (!title) throw new Error('Enter a project title.');
      const now = Date.now();
      const project: DesignToAppProject = {
        id: crypto.randomUUID(),
        title: title.slice(0, 300),
        description: input.description?.trim().slice(0, 8_000) ?? '',
        repositorySlug: input.repositorySlug?.trim().slice(0, 300) ?? '',
        previewUrl: '',
        sources: [],
        verificationCommandIds: [],
        status: 'draft',
        plan: null,
        worktree: null,
        patch: null,
        verification: [],
        beforeEvidence: null,
        afterEvidence: null,
        implementationSummary: null,
        error: null,
        createdAtMs: now,
        updatedAtMs: now,
      };
      set((state) => {
        const projects = upsert(state.projects, project);
        persist(projects);
        return { projects, selectedProjectId: project.id };
      });
      return project;
    },

    updateProject: (projectId, patch) => {
      const current = get().projects.find((project) => project.id === projectId);
      if (!current) throw new Error('Unknown Design-to-App project.');
      const normalized: Partial<EditableProjectFields> = {
        ...(patch.title !== undefined ? { title: patch.title.trim().slice(0, 300) } : {}),
        ...(patch.description !== undefined ? { description: patch.description.trim().slice(0, 8_000) } : {}),
        ...(patch.repositorySlug !== undefined ? { repositorySlug: patch.repositorySlug.trim().slice(0, 300) } : {}),
        ...(patch.previewUrl !== undefined ? { previewUrl: patch.previewUrl.trim().slice(0, 2_000) } : {}),
        ...(patch.verificationCommandIds !== undefined
          ? { verificationCommandIds: [...new Set(patch.verificationCommandIds)].slice(0, 12) }
          : {}),
      };
      if (normalized.title !== undefined && !normalized.title) throw new Error('Enter a project title.');
      if (
        normalized.repositorySlug !== undefined
        && normalized.repositorySlug !== current.repositorySlug
        && current.worktree
      ) {
        throw new Error('Repository cannot change after an owned worktree has been created. Create a new project instead.');
      }
      const planChanged = (normalized.title !== undefined && normalized.title !== current.title)
        || (normalized.description !== undefined && normalized.description !== current.description);
      const previewChanged = normalized.previewUrl !== undefined && normalized.previewUrl !== current.previewUrl;
      if (planChanged) {
        resetGenerated(projectId, {
          ...normalized,
        });
      } else {
        update(projectId, {
          ...normalized,
          ...(previewChanged ? { beforeEvidence: null, afterEvidence: null } : {}),
          error: null,
        });
      }
    },

    addSource: (projectId, source) => {
      const current = get().projects.find((project) => project.id === projectId);
      if (!current) throw new Error('Unknown Design-to-App project.');
      const sources = [...current.sources.filter((item) => item.id !== source.id), source];
      const errors = api.validateDesignSources(sources).filter((error) => !error.startsWith('A Figma URL'));
      if (sources.length > api.MAX_DESIGN_SOURCES || errors.some((error) => /at most|exceed/i.test(error))) {
        throw new Error(errors.join(' ') || `Use at most ${api.MAX_DESIGN_SOURCES} design sources.`);
      }
      resetGenerated(projectId, { sources });
    },

    replaceSource: (projectId, sourceId, source) => {
      const current = get().projects.find((project) => project.id === projectId);
      if (!current) throw new Error('Unknown Design-to-App project.');
      if (!current.sources.some((item) => item.id === sourceId)) throw new Error('Unknown design source.');
      const sources = current.sources.map((item) => item.id === sourceId ? { ...source, id: sourceId } : item);
      resetGenerated(projectId, { sources });
    },

    removeSource: (projectId, sourceId) => {
      const current = get().projects.find((project) => project.id === projectId);
      if (!current) return;
      resetGenerated(projectId, { sources: current.sources.filter((source) => source.id !== sourceId) });
    },

    analyze: async (projectId) => {
      const project = get().projects.find((candidate) => candidate.id === projectId);
      if (!project) throw new Error('Unknown Design-to-App project.');
      if (controllers.has(projectId)) throw new Error('This project already has a run in progress.');
      const controller = new AbortController();
      controllers.set(projectId, controller);
      update(projectId, { status: 'planning', error: null, plan: null });
      try {
        const result = await api.analyzeDesignToApp({
          projectId,
          title: project.title,
          description: project.description,
          sources: project.sources,
          signal: controller.signal,
          onToolActivity: (activity) => setActivity(projectId, activity),
        });
        if (result.outcome === 'completed' && result.plan) {
          update(projectId, { status: 'planned', plan: result.plan, error: null });
        } else if (result.outcome === 'cancelled') {
          update(projectId, { status: 'cancelled', error: null });
        } else {
          update(projectId, { status: 'failed', error: result.summary });
        }
      } catch (error) {
        update(projectId, {
          status: controller.signal.aborted ? 'cancelled' : 'failed',
          error: controller.signal.aborted ? null : errorText(error),
        });
      } finally {
        controllers.delete(projectId);
        setActivity(projectId, null);
      }
    },

    run: async (projectId) => {
      const initial = get().projects.find((candidate) => candidate.id === projectId);
      if (!initial) throw new Error('Unknown Design-to-App project.');
      if (!initial.plan) throw new Error('Generate and review a source-mapped plan first.');
      if (!initial.repositorySlug.trim()) throw new Error('Enter the repository as owner/name first.');
      if (controllers.has(projectId)) throw new Error('This project already has a run in progress.');
      const controller = new AbortController();
      controllers.set(projectId, controller);
      try {
        update(projectId, { status: 'capturing_before', error: null });
        await capture(initial, 'before', controller.signal);
        if (controller.signal.aborted) {
          update(projectId, { status: 'cancelled' });
          return;
        }

        let worktree = initial.worktree;
        if (!worktree) {
          update(projectId, { status: 'creating_worktree' });
          worktree = await api.createDesignToAppWorktree({
            title: initial.title,
            repositorySlug: initial.repositorySlug,
          });
        } else {
          worktree = await api.ensureDesignWorktreeAttached(worktree);
        }
        update(projectId, { worktree });
        if (controller.signal.aborted) {
          update(projectId, { status: 'cancelled' });
          return;
        }

        update(projectId, { status: 'implementing' });
        const configured = useVerifyStore.getState().config.commands;
        const selectedIds = new Set(initial.verificationCommandIds);
        const verificationCommands = configured.filter(
          (command) => command.enabled && selectedIds.has(command.id),
        );
        const result = await api.runDesignToAppImplementation({
          projectId,
          title: initial.title,
          plan: initial.plan,
          sources: initial.sources,
          worktree,
          verificationCommands,
          signal: controller.signal,
          onToolActivity: (activity) => setActivity(projectId, activity),
        });
        update(projectId, {
          patch: result.patch,
          verification: result.verification,
          implementationSummary: result.summary,
        });
        if (result.outcome === 'cancelled' || controller.signal.aborted) {
          update(projectId, { status: 'cancelled', error: null });
          return;
        }

        update(projectId, { status: 'capturing_after' });
        await capture({ ...initial, worktree }, 'after', controller.signal);
        if (result.outcome === 'completed') {
          update(projectId, { status: 'completed', error: null });
        } else {
          update(projectId, { status: 'failed', error: result.summary });
        }
      } catch (error) {
        update(projectId, {
          status: controller.signal.aborted ? 'cancelled' : 'failed',
          error: controller.signal.aborted ? null : errorText(error),
        });
      } finally {
        controllers.delete(projectId);
        setActivity(projectId, null);
      }
    },

    captureEvidence: async (projectId, phase) => {
      const project = get().projects.find((candidate) => candidate.id === projectId);
      if (!project) throw new Error('Unknown Design-to-App project.');
      if (controllers.has(projectId)) throw new Error('Wait for the active run to finish before recapturing evidence.');
      const controller = new AbortController();
      controllers.set(projectId, controller);
      try {
        await capture(project, phase, controller.signal);
      } finally {
        controllers.delete(projectId);
        setActivity(projectId, null);
      }
    },

    cancel: (projectId) => controllers.get(projectId)?.abort(),

    clearError: (projectId) => update(projectId, { error: null }),

    deleteProject: (projectId) => {
      controllers.get(projectId)?.abort();
      controllers.delete(projectId);
      set((state) => {
        const projects = state.projects.filter((project) => project.id !== projectId);
        persist(projects);
        return {
          projects,
          selectedProjectId: state.selectedProjectId === projectId ? projects[0]?.id ?? null : state.selectedProjectId,
        };
      });
    },
  };
});

export default useDesignToAppStore;
