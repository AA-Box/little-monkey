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
/** One `MONKEY.md` rules/memory file — `rules::RULES_REVISION_KIND`. */
export const RULES_KIND = "rules";
/** The whole `mcp_servers.json` document — `mcp::MCP_CONFIG_REVISION_KIND`. */
export const MCP_CONFIG_KIND = "mcp-config";
/** One configured MCP server — `mcp::MCP_SERVER_REVISION_KIND`. */
export const MCP_SERVER_KIND = "mcp-server";
/** The one entity id the MCP config document is filed under. */
export const MCP_CONFIG_ENTITY = "servers";

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
  /** See {@link RevisionMeta.changeId}. */
  changeId: string | null;
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
  /**
   * The write this revision came from, shared with every other revision the
   * same save produced — across entities and kinds. `null` on revisions written
   * before change ids existed: those are uncorrelated, and must be shown as
   * such rather than grouped with whatever was saved around the same second.
   */
  changeId: string | null;
}

/** One entity's part in a change (`config_revisions::ChangeEntry`). */
export interface ChangeEntry {
  kind: string;
  entityId: string;
  revision: RevisionMeta;
}

/** Everything one write touched, across entities and kinds. */
export interface ChangeSet {
  /** `null` for a revision that predates change ids — see {@link RevisionMeta.changeId}. */
  changeId: string | null;
  createdAt: number;
  entries: ChangeEntry[];
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
 * What one change touched, across kinds — the read the per-entity history
 * cannot do. Pass a `changeId` for one change; omit it for the most recent
 * ones. A revision with no change id comes back as its own single-entry set
 * with `changeId: null`, never folded into a neighbouring group.
 */
export async function listChanges(changeId?: string | null, limit?: number): Promise<ChangeSet[]> {
  if (unavailable()) return [];
  return await invoke<ChangeSet[]>("config_revisions_changes", {
    changeId: changeId ?? null,
    limit: limit ?? null,
  });
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

/**
 * The revision entity id for one rules file.
 *
 * Asked of the backend rather than derived here, and that is deliberate: the id
 * is built from the file's *resolved* path (two attached roots can both be
 * labelled `src`), and re-deriving that rule in TypeScript is how a conflict
 * check ends up silently checking a different file's log.
 */
export async function rulesRevisionEntity(
  scope: "global" | "project",
  rootPath: string | null,
): Promise<string | null> {
  if (unavailable()) return null;
  return await invoke<string>("rules_revision_entity", { scope, rootPath });
}

/** The revision a rules editor should save against, or `null` if none yet. */
export async function rulesCurrentRevision(
  scope: "global" | "project",
  rootPath: string | null,
): Promise<string | null> {
  if (unavailable()) return null;
  return await invoke<string | null>("rules_current_revision", { scope, rootPath });
}

/** The revision an MCP config mutation should save against. */
export async function mcpCurrentRevision(): Promise<string | null> {
  if (unavailable()) return null;
  return await invoke<string | null>("mcp_current_revision");
}

/**
 * Puts a whole `mcp_servers.json` snapshot back, through the ordinary save path
 * so the restore is itself recorded.
 *
 * The snapshot is validated backend-side before anything is written, so a
 * hand-edited revision is refused rather than installed and discovered at the
 * next connect.
 */
export async function restoreMcpConfig(snapshot: string): Promise<string> {
  return await invoke<string>("mcp_restore_config", { snapshot });
}
