import { beforeEach, describe, expect, it, vi } from 'vitest';

const apiMocks = vi.hoisted(() => ({
  analyzeDesignToApp: vi.fn(),
  createDesignToAppWorktree: vi.fn(),
  ensureDesignWorktreeAttached: vi.fn(),
  runDesignToAppImplementation: vi.fn(),
  captureDesignBrowserEvidence: vi.fn(),
}));

vi.mock('../lib/designToApp', async (importActual) => {
  const actual = await importActual<typeof import('../lib/designToApp')>();
  return { ...actual, ...apiMocks };
});

const verifyState = vi.hoisted(() => ({
  config: {
    commands: [
      { id: 'test', label: 'Tests', command: 'pnpm test', kind: 'test', enabled: true },
      { id: 'disabled', label: 'Disabled', command: 'pnpm lint', kind: 'lint', enabled: false },
    ],
  },
}));

vi.mock('./verifyStore', () => ({
  useVerifyStore: { getState: () => verifyState },
}));

import { createLocalDesignSource, designSourceRevision, type DesignImplementationPlan } from '../lib/designToApp';
import {
  __resetDesignToAppControllersForTests,
  DESIGN_TO_APP_STORAGE_KEY,
  useDesignToAppStore,
} from './designToAppStore';

const storage = new Map<string, string>();

function imageSource() {
  return createLocalDesignSource({
    id: 'shot',
    kind: 'screenshot',
    name: 'screen.png',
    mediaType: 'image/png',
    imageDataUrl: 'data:image/png;base64,AAAA',
  });
}

function plan(): DesignImplementationPlan {
  return {
    planId: 'plan-1',
    sourceRevision: designSourceRevision([imageSource()]),
    summary: 'Plan',
    routes: [{ routeId: 'home', path: '/', purpose: 'Home', sourceIds: ['shot'] }],
    components: [],
    tokens: [],
    steps: [{ stepId: 'one', title: 'Build', details: 'Build', expectedFiles: [], acceptanceCriteria: [], sourceIds: ['shot'] }],
    accessibilityChecklist: [],
    verificationHints: [],
    generatedAtMs: 1,
    durableRunId: 'plan-run',
  };
}

beforeEach(() => {
  storage.clear();
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => storage.set(key, value),
    removeItem: (key: string) => storage.delete(key),
  });
  __resetDesignToAppControllersForTests();
  for (const mock of Object.values(apiMocks)) mock.mockReset();
  useDesignToAppStore.setState({
    projects: [],
    selectedProjectId: null,
    activityByProject: {},
  });
});

describe('useDesignToAppStore', () => {
  it('persists project history while stripping live image bytes', () => {
    const project = useDesignToAppStore.getState().createProject({ title: 'Landing' });
    useDesignToAppStore.getState().addSource(project.id, imageSource());

    const live = useDesignToAppStore.getState().projects[0].sources[0];
    const persisted = JSON.parse(storage.get(DESIGN_TO_APP_STORAGE_KEY)!).projects[0].sources[0];
    expect(live.imageDataUrl).toContain('data:image/png');
    expect(persisted.imageDataUrl).toBeNull();
  });

  it('moves a project through planning and keeps the durable plan', async () => {
    const project = useDesignToAppStore.getState().createProject({ title: 'Landing' });
    useDesignToAppStore.getState().addSource(project.id, imageSource());
    apiMocks.analyzeDesignToApp.mockResolvedValue({
      outcome: 'completed',
      summary: 'Plan',
      durableRunId: 'plan-run',
      plan: plan(),
    });

    await useDesignToAppStore.getState().analyze(project.id);

    expect(useDesignToAppStore.getState().projects[0]).toMatchObject({
      status: 'planned',
      plan: { planId: 'plan-1', durableRunId: 'plan-run' },
    });
  });

  it('runs baseline, owned worktree implementation, selected checks, and after evidence in order', async () => {
    const project = useDesignToAppStore.getState().createProject({
      title: 'Landing',
      repositorySlug: 'acme/app',
    });
    useDesignToAppStore.getState().addSource(project.id, imageSource());
    useDesignToAppStore.setState((state) => ({
      projects: state.projects.map((item) => item.id === project.id ? {
        ...item,
        previewUrl: 'http://localhost:5173',
        plan: plan(),
        status: 'planned' as const,
        verificationCommandIds: ['test', 'disabled'],
      } : item),
    }));
    apiMocks.captureDesignBrowserEvidence
      .mockResolvedValueOnce({ phase: 'before', status: 'captured', url: 'http://localhost:5173', screenshotArtifactId: 'before', artifactIds: ['before'], accessibilityIssues: [], error: null, capturedAtMs: 1 })
      .mockResolvedValueOnce({ phase: 'after', status: 'captured', url: 'http://localhost:5173', screenshotArtifactId: 'after', artifactIds: ['after'], accessibilityIssues: [], error: null, capturedAtMs: 2 });
    apiMocks.createDesignToAppWorktree.mockResolvedValue({
      worktreeId: 'wt-1', branch: 'design-to-app/landing', workspaceLabel: 'landing-wt', canonicalPath: '/tmp/landing-wt',
    });
    apiMocks.runDesignToAppImplementation.mockResolvedValue({
      outcome: 'completed',
      summary: 'Built landing route.',
      durableRunId: 'build-run',
      patch: { files: ['src/App.tsx'], diff: '+route', truncated: false },
      verification: [{ commandId: 'test', label: 'Tests', command: 'pnpm test', status: 'passed', exitCode: 0, output: 'ok', durationMs: 10, durableRunId: 'verify-run' }],
    });

    await useDesignToAppStore.getState().run(project.id);

    const finished = useDesignToAppStore.getState().projects[0];
    expect(finished.status).toBe('completed');
    expect(finished.beforeEvidence?.screenshotArtifactId).toBe('before');
    expect(finished.afterEvidence?.screenshotArtifactId).toBe('after');
    expect(finished.worktree?.branch).toBe('design-to-app/landing');
    expect(finished.patch?.files).toEqual(['src/App.tsx']);
    expect(apiMocks.runDesignToAppImplementation).toHaveBeenCalledWith(expect.objectContaining({
      verificationCommands: [expect.objectContaining({ id: 'test' })],
    }));
  });

  it('does not invalidate a plan when unchanged controlled fields blur', () => {
    const project = useDesignToAppStore.getState().createProject({ title: 'Landing', description: 'Goal' });
    useDesignToAppStore.setState((state) => ({
      projects: state.projects.map((item) => item.id === project.id ? { ...item, plan: plan(), status: 'planned' as const } : item),
    }));

    useDesignToAppStore.getState().updateProject(project.id, {
      title: 'Landing',
      description: 'Goal',
      repositorySlug: '',
      previewUrl: '',
    });

    expect(useDesignToAppStore.getState().projects[0].plan?.planId).toBe('plan-1');
  });
});
