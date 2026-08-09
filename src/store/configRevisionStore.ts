import { invoke, isTauri } from "@tauri-apps/api/core";

/**
 * Client for the local revision history of everything the user authors —
 * personas, snippets, skills, and workflow definitions (roadmap K24 /
 * ROADMAP #3). The store itself lives in Rust (`src-tauri/src/config_revisions.rs`);
 * this module is the typed seam plus the two rules the UI must not get wrong:
 * how a conflict is recognized, and what a "revision" means when the app is
 * running outside the Tauri shell.
 */

/** Revision kinds, matching the Rust constants they are keyed by. */
export const PROMPT_ENTRY_KIND = "prompt";
export const PROMPT_LIBRARY_KIND = "prompt-library";
export const WORKFLOW_KIND = "workflow";

/** The branch every entity starts on (`config_revisions::DEFAULT_BRANCH`). */
export const DEFAULT_BRANCH = "main";

export interface Revision {
  revisionId: string;
  parentId: string | null;
  branch: string;
  sequence: number;
  createdAt: number;
  label: string;
  contentSha256: string;
  entityId: string;
  content: string;
}

/** A revision without its snapshot — what the history list renders. */
export interface RevisionMeta {
  revisionId: string;
  parentId: string | null;
  branch: string;
  sequence: number;
  createdAt: number;
  label: string;
  contentSha256: string;
  bytes: number;
}

export interface BranchSummary {
  name: string;
  headRevisionId: string;
  revisionCount: number;
  updatedAt: number;
}

/**
 * Whether a failed save failed because someone else saved first.
 *
 * A `#[tauri::command]` can only reject with a string, so the backend prefixes
 * exactly this case — see `RevisionError::Conflict`'s `Display`. Matching on
 * the prefix (rather than on any error at all) is what keeps a disk-full or
 * permission failure from being reported to the user as a phantom concurrent
 * edit. `WorkflowService::update` uses the same prefix for the same reason.
 */
export function isConflictError(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  return message.toLowerCase().includes("conflict:");
}

/**
 * Outside the Tauri shell (plain `vite` dev) there is no revision store at
 * all. Every read below resolves empty rather than throwing, so a browser-only
 * dev session renders the panel's empty state instead of an error banner —
 * the same stance `promptStore.ts` takes toward its own file persistence.
 */
function unavailable(): boolean {
  return !isTauri();
}

export async function listRevisions(
  kind: string,
  entityId: string,
  branch?: string,
): Promise<RevisionMeta[]> {
  if (unavailable()) return [];
  return await invoke<RevisionMeta[]>("config_revisions_history", { kind, entityId, branch: branch ?? null });
}

export async function getRevision(kind: string, entityId: string, revisionId: string): Promise<Revision> {
  return await invoke<Revision>("config_revisions_get", { kind, entityId, revisionId });
}

export async function headRevision(
  kind: string,
  entityId: string,
  branch?: string,
): Promise<Revision | null> {
  if (unavailable()) return null;
  return await invoke<Revision | null>("config_revisions_head", { kind, entityId, branch: branch ?? null });
}

export async function listBranches(kind: string, entityId: string): Promise<BranchSummary[]> {
  if (unavailable()) return [];
  return await invoke<BranchSummary[]>("config_revisions_branches", { kind, entityId });
}

export async function listVersionedEntities(kind: string): Promise<string[]> {
  if (unavailable()) return [];
  return await invoke<string[]>("config_revisions_entities", { kind });
}

/**
 * Records a revision. Pass `baseRevisionId` to opt into the concurrent-edit
 * check — the write is refused (see `isConflictError`) if that is no longer
 * the branch head.
 */
export async function recordRevision(args: {
  kind: string;
  entityId: string;
  label: string;
  content: string;
  branch?: string;
  baseRevisionId?: string | null;
}): Promise<Revision> {
  return await invoke<Revision>("config_revisions_record", {
    kind: args.kind,
    entityId: args.entityId,
    label: args.label,
    content: args.content,
    branch: args.branch ?? null,
    baseRevisionId: args.baseRevisionId ?? null,
  });
}

/** Forks a named branch from an existing revision, so two variants can be
 * kept and compared instead of one overwriting the other. */
export async function branchFromRevision(
  kind: string,
  entityId: string,
  fromRevisionId: string,
  newBranch: string,
): Promise<Revision> {
  return await invoke<Revision>("config_revisions_branch", {
    kind,
    entityId,
    fromRevisionId,
    newBranch,
  });
}

/** `^[a-z0-9._-]{1,48}$` — mirrors `config_revisions::validate_branch`, so the
 * form can reject a bad name before a round trip. */
export function isValidBranchName(name: string): boolean {
  return /^[a-z0-9._-]{1,48}$/.test(name);
}
