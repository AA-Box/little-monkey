/**
 * Permission-Aware Universal Search (ROADMAP.md Phase 7, "Permission-Aware
 * Universal Search"): pure, store-free functions that fan a query out across
 * every source this app already tracks in memory — chat sessions, run/task
 * history, workspace files, knowledge stacks, browser workbench evidence, and
 * connected MCP servers — and filter each source's matches through the
 * app's existing access boundaries (workspace root attachment, connector
 * connection state) BEFORE a hit is ever returned.
 *
 * Mirrors `lib/inbox.ts` / `store/dailyBriefStore.ts`'s own split: the
 * matching/filtering logic worth getting right lives here as plain data
 * transforms (no Tauri/zustand imports), so it's unit-testable without
 * mocking the app shell. `store/universalSearchStore.ts` is the only piece
 * that talks to the other stores and the Tauri `tool_grep` command, and it
 * does so by handing already-fetched arrays to the builders below — a
 * result never reaches `GlobalSearch.tsx` without passing through the
 * relevant `isPathWithinRoots`/connection-state check first, i.e. content
 * the current workspace/device grant doesn't cover is dropped here, not
 * merely hidden after the fact by the UI layer.
 */

import { textContent, type ChatMessage } from "./llamaClient";
import type { ChatSession } from "../store/sessionStore";
import type { RunRecord } from "./runProtocol";
import type { BrowserChatEvidence } from "../store/browserWorkbenchStore";

export type UniversalSearchSourceKind =
  | "session"
  | "workspace_file"
  | "task"
  | "knowledge"
  | "browser_evidence"
  | "connected_app";

export interface UniversalSearchHit {
  id: string;
  sourceKind: UniversalSearchSourceKind;
  title: string;
  snippet: string;
  occurredAtMs: number;
  sessionId: string | null;
  runId: string | null;
  workspacePath: string | null;
  archived: boolean;
}

/** One source's matches plus how many additional matches were dropped by a
 * permission/access check — never a count of "didn't match the query",
 * only of "matched, but the current device/workspace grant doesn't cover
 * it". */
export interface UniversalSearchResult {
  hits: UniversalSearchHit[];
  excludedCount: number;
}

export function combineUniversalSearchResults(
  results: readonly UniversalSearchResult[],
): UniversalSearchResult {
  return {
    hits: results.flatMap((result) => result.hits),
    excludedCount: results.reduce((sum, result) => sum + result.excludedCount, 0),
  };
}

// ---------------------------------------------------------------------------
// Text matching helpers
// ---------------------------------------------------------------------------

export function matchesQuery(haystack: string, query: string): boolean {
  const needle = query.trim().toLowerCase();
  if (!needle) return false;
  return haystack.toLowerCase().includes(needle);
}

/** Builds a short excerpt centered on the first occurrence of `query`, with
 * an ellipsis on whichever side got cut off. Falls back to the start of
 * `haystack` if the query isn't literally present (e.g. the match came from
 * a different field, like a session title vs. its messages). */
export function buildSnippet(haystack: string, query: string, radius = 60): string {
  const needle = query.trim().toLowerCase();
  const idx = needle ? haystack.toLowerCase().indexOf(needle) : -1;
  if (idx === -1) {
    const truncated = haystack.slice(0, radius * 2).trim();
    return haystack.length > radius * 2 ? `${truncated}…` : truncated;
  }
  const start = Math.max(0, idx - radius);
  const end = Math.min(haystack.length, idx + needle.length + radius);
  const prefix = start > 0 ? "…" : "";
  const suffix = end < haystack.length ? "…" : "";
  return `${prefix}${haystack.slice(start, end).trim()}${suffix}`;
}

/** Escapes regex metacharacters so a plain-text search query can be sent to
 * `tool_grep` (which takes a regex pattern) without the query itself being
 * interpreted as one. */
export function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// ---------------------------------------------------------------------------
// Workspace root boundary
// ---------------------------------------------------------------------------

export interface WorkspaceRootLike {
  path: string;
}

/** True when `path` is null (never claimed a workspace, so there's nothing
 * to bound it by) or falls under one of the currently attached `roots` —
 * false when it names a workspace that was open once but isn't attached to
 * this device/session right now, which is the "workspace root boundary"
 * this feature must respect: a session/run/evidence item created under a
 * workspace that isn't currently open must not leak its content until that
 * workspace is reattached. */
export function isPathWithinRoots(path: string | null, roots: readonly WorkspaceRootLike[]): boolean {
  if (path == null) return true;
  return roots.some((root) => path === root.path || path.startsWith(`${root.path}/`));
}

// ---------------------------------------------------------------------------
// Chat sessions
// ---------------------------------------------------------------------------

function sessionHaystackParts(session: ChatSession): string[] {
  return [session.title, ...session.messages.map((message: ChatMessage) => textContent(message.content))];
}

export function buildSessionHits(
  sessions: readonly ChatSession[],
  query: string,
  roots: readonly WorkspaceRootLike[],
  includeArchived: boolean,
): UniversalSearchResult {
  let excludedCount = 0;
  const hits: UniversalSearchHit[] = [];
  for (const session of sessions) {
    if (session.archived && !includeArchived) continue;
    const parts = sessionHaystackParts(session);
    const matchedPart = parts.find((part) => matchesQuery(part, query));
    if (matchedPart === undefined) continue;
    if (!isPathWithinRoots(session.workspacePath, roots)) {
      excludedCount += 1;
      continue;
    }
    hits.push({
      id: `session:${session.id}`,
      sourceKind: "session",
      title: session.title || "Untitled chat",
      snippet: buildSnippet(matchedPart, query),
      occurredAtMs: session.updatedAt,
      sessionId: session.id,
      runId: null,
      workspacePath: session.workspacePath,
      archived: session.archived,
    });
  }
  return { hits, excludedCount };
}

// ---------------------------------------------------------------------------
// Workspace files (backed by the existing `tool_grep` Tauri command, which
// already sandboxes every search to an attached workspace root via
// `workspace::resolve_path_and_root` — content outside an attached root is
// never returned by the backend in the first place, so there is nothing to
// additionally filter or count as excluded here).
// ---------------------------------------------------------------------------

export interface GrepMatchLike {
  file: string;
  line: number;
  text: string;
}

export function buildWorkspaceFileHits(
  matches: readonly GrepMatchLike[],
  query: string,
  limit = 50,
): UniversalSearchResult {
  const hits = matches.slice(0, limit).map((match) => ({
    id: `workspace_file:${match.file}:${match.line}`,
    sourceKind: "workspace_file" as const,
    title: match.file,
    snippet: buildSnippet(match.text, query, 80),
    occurredAtMs: 0,
    sessionId: null,
    runId: null,
    workspacePath: null,
    archived: false,
  }));
  return { hits, excludedCount: 0 };
}

// ---------------------------------------------------------------------------
// Run/task history
// ---------------------------------------------------------------------------

export function buildTaskHits(
  runs: readonly RunRecord[],
  query: string,
  roots: readonly WorkspaceRootLike[],
  includeArchived: boolean,
): UniversalSearchResult {
  let excludedCount = 0;
  const hits: UniversalSearchHit[] = [];
  for (const run of runs) {
    if (run.archivedAtMs != null && !includeArchived) continue;
    const haystack = [run.spec.task, run.spec.instructions ?? "", run.spec.target.label].join("\n");
    if (!matchesQuery(haystack, query)) continue;
    const rootPaths = run.spec.workspace?.roots.map((root) => root.canonical_path) ?? [];
    const accessible = rootPaths.length === 0 || rootPaths.some((path) => isPathWithinRoots(path, roots));
    if (!accessible) {
      excludedCount += 1;
      continue;
    }
    hits.push({
      id: `task:${run.spec.run_id}`,
      sourceKind: "task",
      title: run.spec.task || run.spec.run_id,
      snippet: buildSnippet(haystack, query),
      occurredAtMs: run.updatedAtMs,
      sessionId: null,
      runId: run.spec.run_id,
      workspacePath: rootPaths[0] ?? null,
      archived: run.archivedAtMs != null,
    });
  }
  return { hits, excludedCount };
}

// ---------------------------------------------------------------------------
// Knowledge stacks
// ---------------------------------------------------------------------------

export interface KnowledgeQueryHitLike {
  stackId: string;
  stackName: string;
  sourcePath: string;
  text: string;
}

/** No exclusion counting needed here: the caller only ever queries stacks
 * that are already configured locally on this device (`useStackStore`'s own
 * list), so every candidate hit is, by construction, something this device
 * is already allowed to read — there's no broader universe of "stacks that
 * matched but aren't accessible" to drop. */
export function buildKnowledgeHits(
  hits: readonly KnowledgeQueryHitLike[],
  query: string,
  nowMs: number = Date.now(),
): UniversalSearchResult {
  const seen = new Set<string>();
  const built: UniversalSearchHit[] = [];
  for (const hit of hits) {
    const id = `knowledge:${hit.stackId}:${hit.sourcePath}`;
    if (seen.has(id)) continue;
    seen.add(id);
    built.push({
      id,
      sourceKind: "knowledge",
      title: `${hit.stackName} — ${hit.sourcePath}`,
      snippet: buildSnippet(hit.text, query),
      occurredAtMs: nowMs,
      sessionId: null,
      runId: null,
      workspacePath: null,
      archived: false,
    });
  }
  return { hits: built, excludedCount: 0 };
}

// ---------------------------------------------------------------------------
// Browser workbench evidence
// ---------------------------------------------------------------------------

export function buildBrowserEvidenceHits(
  evidenceBySession: Readonly<Record<string, BrowserChatEvidence>>,
  sessionsById: ReadonlyMap<string, ChatSession>,
  query: string,
  roots: readonly WorkspaceRootLike[],
): UniversalSearchResult {
  let excludedCount = 0;
  const hits: UniversalSearchHit[] = [];
  for (const [sessionId, evidence] of Object.entries(evidenceBySession)) {
    if (!matchesQuery(evidence.summary, query)) continue;
    const session = sessionsById.get(sessionId);
    // Evidence whose owning session no longer exists, or whose workspace
    // isn't currently attached, is dropped rather than shown orphaned or
    // out-of-boundary.
    if (!session || !isPathWithinRoots(session.workspacePath, roots)) {
      excludedCount += 1;
      continue;
    }
    hits.push({
      id: `browser_evidence:${evidence.id}`,
      sourceKind: "browser_evidence",
      title: session.title || "Browser evidence",
      snippet: buildSnippet(evidence.summary, query, 80),
      occurredAtMs: session.updatedAt,
      sessionId,
      runId: null,
      workspacePath: session.workspacePath,
      archived: session.archived,
    });
  }
  return { hits, excludedCount };
}

// ---------------------------------------------------------------------------
// Connected apps (MCP servers) — the "connected apps" search source. This
// repo's connector-catalog primitive (OAuth-style third-party connectors)
// hadn't landed yet as of this feature; MCP servers are the one already-real
// "connect this app, grant it scope, use its tools" surface in the app
// today, so they stand in for it here. A server's tools are only
// searchable while it's both enabled and actually connected — matching the
// acceptance line that a result must reflect a device grant the app
// currently has, not one it merely has on file.
// ---------------------------------------------------------------------------

export interface McpToolLike {
  name: string;
  description: string | null;
}

export interface McpServerLike {
  id: string;
  label: string;
  enabled: boolean;
  status: string;
  tools: readonly McpToolLike[];
}

export function buildConnectedAppHits(
  servers: readonly McpServerLike[],
  query: string,
  nowMs: number = Date.now(),
): UniversalSearchResult {
  let excludedCount = 0;
  const hits: UniversalSearchHit[] = [];
  for (const server of servers) {
    const matchedTool = server.tools.find(
      (tool) => matchesQuery(tool.name, query) || matchesQuery(tool.description ?? "", query),
    );
    const labelMatches = matchesQuery(server.label, query);
    if (!labelMatches && !matchedTool) continue;
    const connected = server.enabled && server.status === "connected";
    if (!connected) {
      excludedCount += 1;
      continue;
    }
    const snippetSource = matchedTool
      ? `${matchedTool.name}${matchedTool.description ? `: ${matchedTool.description}` : ""}`
      : server.label;
    hits.push({
      id: `connected_app:${server.id}`,
      sourceKind: "connected_app",
      title: server.label,
      snippet: buildSnippet(snippetSource, query),
      occurredAtMs: nowMs,
      sessionId: null,
      runId: null,
      workspacePath: null,
      archived: false,
    });
  }
  return { hits, excludedCount };
}
