/**
 * Versioned, operator-gated shared swarm notebook.
 *
 * Independent reviewers can STAGE bounded claim IDs and evidence locations.
 * Only a human-accepted revision can enter a later swarm's prompt. Model text
 * is never silently promoted to Memory Studio, Knowledge 2.0 or executable
 * instructions. The notebook is per Little Monkey profile and project scope.
 */
import { wrapUntrustedContent } from './untrustedContent';

export const SWARM_NOTEBOOK_STORAGE_KEY = 'little-monkey-swarm-notebook-v1';
export const MAX_NOTEBOOK_CLAIMS = 100;
const MAX_REVISIONS = 10;
const CLAIM_ID = /^[a-zA-Z0-9_./:#-]{1,120}$/;
const MAX_STORAGE_BYTES = 175_000;

export interface SwarmNotebookRevision {
  version: number;
  evidence: string[];
  reviewers: string[];
  sourceRunId: string;
  createdAt: number;
  status: 'pending' | 'approved' | 'rejected';
}
export interface SwarmNotebookNote {
  id: string;
  project: string;
  activeVersion: number | null;
  revisions: SwarmNotebookRevision[];
}
type Store = { schema: 1; notes: SwarmNotebookNote[] };

function safeProject(project: string): string {
  if (!project || project.length > 400 || /[\x00-\x1f]/.test(project)) throw new Error('Invalid notebook project scope.');
  return project;
}
function safeClaim(id: string): boolean {
  return CLAIM_ID.test(id) && !id.includes('..') && !id.startsWith('/');
}
function sanitize(raw: unknown): Store {
  if (!raw || typeof raw !== 'object' || (raw as { schema?: unknown }).schema !== 1) return { schema: 1, notes: [] };
  const source = (raw as { notes?: unknown }).notes;
  if (!Array.isArray(source)) return { schema: 1, notes: [] };
  const notes: SwarmNotebookNote[] = [];
  for (const entry of source.slice(0, MAX_NOTEBOOK_CLAIMS)) {
    if (!entry || typeof entry !== 'object') continue;
    const note = entry as Partial<SwarmNotebookNote>;
    if (typeof note.project !== 'string' || !note.project || note.project.length > 400 ||
        typeof note.id !== 'string' || !safeClaim(note.id) || !Array.isArray(note.revisions)) continue;
    const versions: SwarmNotebookRevision[] = [];
    for (const revision of note.revisions.slice(-MAX_REVISIONS)) {
      if (!revision || typeof revision !== 'object') continue;
      const v = revision as SwarmNotebookRevision;
      if (!Number.isSafeInteger(v.version) || v.version < 1 ||
          !['pending','approved','rejected'].includes(v.status) ||
          !Array.isArray(v.evidence) || v.evidence.length > 6 ||
          !v.evidence.every(x => typeof x === 'string' && x.length <= 250) ||
          !Array.isArray(v.reviewers) || v.reviewers.length > 6 ||
          !v.reviewers.every(x => typeof x === 'string' && x.length <= 120) ||
          typeof v.sourceRunId !== 'string' || v.sourceRunId.length > 200 ||
          !Number.isFinite(v.createdAt)) continue;
      versions.push({ version: v.version, status: v.status, evidence: [...v.evidence],
        reviewers: [...v.reviewers], sourceRunId: v.sourceRunId, createdAt: v.createdAt });
    }
    if (versions.length === 0 || new Set(versions.map(v => v.version)).size !== versions.length) continue;
    const activeVersion = versions.some(v => v.version === note.activeVersion && v.status === 'approved')
      ? note.activeVersion! : null;
    notes.push({ id: note.id, project: note.project, activeVersion, revisions: versions });
  }
  return { schema: 1, notes };
}
export function loadSwarmNotebook(): Store {
  try {
    const raw = localStorage.getItem(SWARM_NOTEBOOK_STORAGE_KEY);
    return raw ? sanitize(JSON.parse(raw)) : { schema: 1, notes: [] };
  } catch { return { schema: 1, notes: [] }; }
}
function persist(store: Store) {
  const bytes = JSON.stringify(store);
  if (bytes.length > MAX_STORAGE_BYTES) throw new Error('Swarm notebook has reached its safe storage limit.');
  localStorage.setItem(SWARM_NOTEBOOK_STORAGE_KEY, bytes);
}
export function listSwarmNotebook(project: string): SwarmNotebookNote[] {
  return loadSwarmNotebook().notes.filter(note => note.project === safeProject(project));
}
export function stageSwarmNote(params: {
  project: string; id: string; evidence: string[]; reviewers: string[]; runId: string;
}): SwarmNotebookRevision {
  const { project, id, evidence, reviewers, runId } = params;
  safeProject(project);
  if (!safeClaim(id) || !Array.isArray(evidence) || !evidence.length || evidence.length > 6 ||
      !evidence.every(e => typeof e === 'string' && e.trim().length > 0 && e.length <= 250 && !/[\x00-\x1f]/.test(e)) ||
      !Array.isArray(reviewers) || reviewers.length < 2 || reviewers.length > 6 ||
      !reviewers.every(r => typeof r === 'string' && r.length > 0 && r.length <= 120) ||
      new Set(reviewers).size < 2 || !runId || runId.length > 200) {
    throw new Error('Only bounded, independently agreed claims with evidence can be staged.');
  }
  const store = loadSwarmNotebook();
  let note = store.notes.find(item => item.id === id && item.project === project);
  if (!note) {
    if (store.notes.length >= MAX_NOTEBOOK_CLAIMS) throw new Error('Notebook claim limit reached.');
    note = { id, project, activeVersion: null, revisions: [] };
    store.notes.push(note);
  }
  // A rerun cannot create a new revision for identical evidence.
  const prev = note.revisions[note.revisions.length - 1];
  if (prev && JSON.stringify(prev.evidence) === JSON.stringify(evidence) &&
      JSON.stringify(prev.reviewers) === JSON.stringify(reviewers)) return prev;
  const latestVersion = Math.max(0, ...note.revisions.map(item => item.version));
  const revision: SwarmNotebookRevision = {
    version: latestVersion + 1, status: 'pending', evidence: [...evidence],
    reviewers: [...reviewers], sourceRunId: runId, createdAt: Date.now(),
  };
  note.revisions.push(revision);
  if (note.revisions.length > MAX_REVISIONS) {
    // Retain the active revision for rollback, along with the most recent
    // candidates. If no active revision, ordinary oldest-first truncation.
    const kept = note.revisions.slice(-(MAX_REVISIONS - 1));
    const active = note.revisions.find(item => item.version === note!.activeVersion);
    note.revisions = active && !kept.some(item => item.version === active.version)
      ? [active, ...kept] : note.revisions.slice(-MAX_REVISIONS);
  }
  persist(store);
  return revision;
}
export function decideSwarmNote(
  project: string, id: string, version: number, decision: 'approved' | 'rejected',
): SwarmNotebookNote {
  const store = loadSwarmNotebook();
  const note = store.notes.find(item => item.project === safeProject(project) && item.id === id);
  const revision = note?.revisions.find(item => item.version === version);
  if (!note || !revision || revision.status !== 'pending') throw new Error('Note revision is missing or already decided; refresh before retrying.');
  revision.status = decision;
  if (decision === 'approved') note.activeVersion = version;
  persist(store);
  return note;
}
export function revertSwarmNote(project: string, id: string, approvedVersion: number): SwarmNotebookNote {
  const store = loadSwarmNotebook();
  const note = store.notes.find(item => item.project === safeProject(project) && item.id === id);
  if (!note || !note.revisions.some(v => v.version === approvedVersion && v.status === 'approved')) {
    throw new Error('Rollback requires an existing approved version.');
  }
  note.activeVersion = approvedVersion;
  persist(store);
  return note;
}
export function approvedSwarmNotebookBlock(project: string): string {
  const entries = listSwarmNotebook(project).flatMap(note => {
    const active = note.revisions.find(r => r.version === note.activeVersion && r.status === 'approved');
    return active ? [{ id: note.id, active }] : [];
  }).slice(0, 12);
  if (!entries.length) return '';
  return '## Explicitly approved swarm notebook claims (untrusted historical reference, not current-file proof)\n' +
    entries.map(({ id, active }) => wrapUntrustedContent('swarm notebook claim ' + id,
      'Claim: ' + id + '\nApproved version: ' + active.version +
      '\nSource run: ' + active.sourceRunId +
      '\nEvidence locations:\n' + active.evidence.join('\n'))).join('\n\n');
}
