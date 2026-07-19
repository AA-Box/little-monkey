import { describe, expect, it } from "vitest";

import {
  buildBrowserEvidenceHits,
  buildConnectedAppHits,
  buildKnowledgeHits,
  buildSessionHits,
  buildTaskHits,
  buildWorkspaceFileHits,
  buildSnippet,
  combineUniversalSearchResults,
  escapeRegExp,
  isPathWithinRoots,
  matchesQuery,
  type McpServerLike,
} from "./universalSearch";
import type { ChatSession } from "../store/sessionStore";
import type { BrowserChatEvidence } from "../store/browserWorkbenchStore";
import type {
  ClientIdentityWire,
  ModelCapabilitiesSnapshotWire,
  ModelTargetSnapshotWire,
  RunRecord,
  RunSpecWire,
  RunStatus,
  WorkspaceContextWire,
} from "./runProtocol";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

function makeSession(id: string, overrides: Partial<ChatSession> = {}): ChatSession {
  const now = Date.now();
  return {
    id,
    title: `session ${id}`,
    messages: [],
    createdAt: now,
    updatedAt: now,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    modelTarget: null,
    comparisonBranch: null,
    workspacePath: null,
    personaId: null,
    attachedStackIds: [],
    docChatMode: false,
    subagentRuns: {},
    ...overrides,
  };
}

const CAPABILITY_UNKNOWN = { state: "unknown" as const, evidence: "n/a" };
const CAPABILITIES: ModelCapabilitiesSnapshotWire = {
  tool_calling: CAPABILITY_UNKNOWN,
  vision: CAPABILITY_UNKNOWN,
  embeddings: CAPABILITY_UNKNOWN,
  structured_output: CAPABILITY_UNKNOWN,
  image_generation: CAPABILITY_UNKNOWN,
  audio: CAPABILITY_UNKNOWN,
  runtime_lifecycle: CAPABILITY_UNKNOWN,
  fim: CAPABILITY_UNKNOWN,
  code_completion: CAPABILITY_UNKNOWN,
  inline_edit: CAPABILITY_UNKNOWN,
  fim_metadata: null,
};

function target(): ModelTargetSnapshotWire {
  return {
    kind: "ollama",
    target_id: "target-1",
    label: "qwen2.5:14b",
    base_url: "http://127.0.0.1:11434",
    model: "qwen2.5:14b",
    is_cloud: false,
    capabilities: CAPABILITIES,
    estimated_memory_bytes: null,
  };
}

function identity(): ClientIdentityWire {
  return { client_id: "little-monkey-desktop", instance_id: "main", kind: "desktop", version: "0.1.0" };
}

function workspaceContext(path: string): WorkspaceContextWire {
  return {
    workspace_id: `workspace-${path}`,
    primary_root_id: "root-1",
    roots: [{ root_id: "root-1", canonical_path: path, access: "read_write", allow_symlinks_within_root: false }],
    repository_policy: null,
  };
}

function makeRun(
  runId: string,
  status: RunStatus,
  overrides: Partial<RunSpecWire> = {},
  extra: Partial<Omit<RunRecord, "spec" | "status">> = {},
): RunRecord {
  const spec: RunSpecWire = {
    schema_version: 1,
    run_id: runId,
    idempotency_key: runId,
    created_at_ms: 1_000,
    kind: "interactive",
    submitted_by: identity(),
    task: `Task for ${runId}`,
    instructions: null,
    input_artifact_ids: [],
    target: target(),
    workspace: null,
    permission_policy: {
      mode: "acceptEdits",
      unattended: false,
      approval_timeout_ms: 300_000,
      default_tool_decision: "prompt",
      tool_rules: [],
      allow_network: false,
      allow_external_mutations: false,
    },
    budgets: {
      wall_time_ms: 1_800_000,
      max_iterations: 32,
      max_model_calls: 64,
      max_tool_calls: 128,
      max_input_tokens: 1_000_000,
      max_output_tokens: 250_000,
      max_cost_micros: null,
      max_artifact_bytes: 268_435_456,
      max_event_count: 20_000,
    },
    ...overrides,
  };
  return {
    spec,
    status,
    lastSequence: 3,
    terminalSequence: null,
    updatedAtMs: 2_000,
    archivedAtMs: null,
    ...extra,
  };
}

function evidence(id: string, summary: string): BrowserChatEvidence {
  return { id, summary, screenshot: null };
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

describe("matchesQuery", () => {
  it("is case-insensitive", () => {
    expect(matchesQuery("Hello World", "world")).toBe(true);
  });

  it("returns false for a blank query", () => {
    expect(matchesQuery("anything", "   ")).toBe(false);
  });
});

describe("buildSnippet", () => {
  it("centers the excerpt on the match with ellipses on both cut sides", () => {
    const haystack = `${"a".repeat(100)}NEEDLE${"b".repeat(100)}`;
    const snippet = buildSnippet(haystack, "needle", 10);
    expect(snippet.startsWith("…")).toBe(true);
    expect(snippet.endsWith("…")).toBe(true);
    expect(snippet.toLowerCase()).toContain("needle");
  });

  it("falls back to a plain truncation when the query isn't literally present", () => {
    expect(buildSnippet("short text", "nomatch", 100)).toBe("short text");
  });
});

describe("escapeRegExp", () => {
  it("escapes regex metacharacters", () => {
    expect(escapeRegExp("a.b*c?")).toBe("a\\.b\\*c\\?");
  });
});

describe("isPathWithinRoots", () => {
  const roots = [{ path: "/Users/dev/project" }];

  it("allows a null path (never claimed a workspace)", () => {
    expect(isPathWithinRoots(null, roots)).toBe(true);
  });

  it("allows an exact root match and a subpath", () => {
    expect(isPathWithinRoots("/Users/dev/project", roots)).toBe(true);
    expect(isPathWithinRoots("/Users/dev/project/src", roots)).toBe(true);
  });

  it("rejects a path outside every attached root", () => {
    expect(isPathWithinRoots("/Users/dev/other-project", roots)).toBe(false);
  });

  it("rejects everything when no root is attached", () => {
    expect(isPathWithinRoots("/Users/dev/project", [])).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

describe("buildSessionHits", () => {
  const roots = [{ path: "/Users/dev/project" }];

  it("matches by title or message content", () => {
    const sessions = [
      makeSession("a", { title: "Refactor the parser" }),
      makeSession("b", { messages: [{ role: "user", content: "please refactor this function" }] }),
      makeSession("c", { title: "unrelated" }),
    ];
    const result = buildSessionHits(sessions, "refactor", roots, false);
    expect(result.hits.map((hit) => hit.sessionId)).toEqual(["a", "b"]);
  });

  it("excludes archived sessions unless includeArchived is set", () => {
    const sessions = [makeSession("a", { title: "refactor", archived: true })];
    expect(buildSessionHits(sessions, "refactor", roots, false).hits).toEqual([]);
    expect(buildSessionHits(sessions, "refactor", roots, true).hits).toHaveLength(1);
  });

  it("drops (and counts) a session whose workspace isn't currently attached", () => {
    const sessions = [makeSession("a", { title: "refactor", workspacePath: "/Users/dev/other" })];
    const result = buildSessionHits(sessions, "refactor", roots, false);
    expect(result.hits).toEqual([]);
    expect(result.excludedCount).toBe(1);
  });

  it("keeps a session with no workspace association", () => {
    const sessions = [makeSession("a", { title: "refactor", workspacePath: null })];
    const result = buildSessionHits(sessions, "refactor", roots, false);
    expect(result.hits).toHaveLength(1);
    expect(result.excludedCount).toBe(0);
  });
});

// ---------------------------------------------------------------------------
// Workspace files
// ---------------------------------------------------------------------------

describe("buildWorkspaceFileHits", () => {
  it("maps grep matches to hits and never reports exclusions", () => {
    const matches = [{ file: "src/foo.ts", line: 12, text: "const needle = 1;" }];
    const result = buildWorkspaceFileHits(matches, "needle");
    expect(result.excludedCount).toBe(0);
    expect(result.hits[0].title).toBe("src/foo.ts");
    expect(result.hits[0].sourceKind).toBe("workspace_file");
  });

  it("caps results at the given limit", () => {
    const matches = Array.from({ length: 10 }, (_, i) => ({ file: `f${i}.ts`, line: 1, text: "needle" }));
    expect(buildWorkspaceFileHits(matches, "needle", 3).hits).toHaveLength(3);
  });
});

// ---------------------------------------------------------------------------
// Tasks (run history)
// ---------------------------------------------------------------------------

describe("buildTaskHits", () => {
  const roots = [{ path: "/Users/dev/project" }];

  it("matches on the run's task text", () => {
    const runs = [makeRun("run-1", "succeeded", { task: "Deploy the widget service" })];
    const result = buildTaskHits(runs, "widget", roots, false);
    expect(result.hits.map((hit) => hit.runId)).toEqual(["run-1"]);
  });

  it("excludes archived runs unless includeArchived is set", () => {
    const runs = [makeRun("run-1", "succeeded", { task: "widget" }, { archivedAtMs: 5_000 })];
    expect(buildTaskHits(runs, "widget", roots, false).hits).toEqual([]);
    expect(buildTaskHits(runs, "widget", roots, true).hits).toHaveLength(1);
  });

  it("drops (and counts) a run scoped to a workspace that isn't attached", () => {
    const runs = [makeRun("run-1", "succeeded", { task: "widget", workspace: workspaceContext("/Users/dev/other") })];
    const result = buildTaskHits(runs, "widget", roots, false);
    expect(result.hits).toEqual([]);
    expect(result.excludedCount).toBe(1);
  });

  it("keeps a run scoped to an attached workspace root", () => {
    const runs = [
      makeRun("run-1", "succeeded", { task: "widget", workspace: workspaceContext("/Users/dev/project") }),
    ];
    const result = buildTaskHits(runs, "widget", roots, false);
    expect(result.hits).toHaveLength(1);
    expect(result.excludedCount).toBe(0);
  });

  it("keeps a run with no workspace association at all", () => {
    const runs = [makeRun("run-1", "succeeded", { task: "widget", workspace: null })];
    expect(buildTaskHits(runs, "widget", roots, false).hits).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// Knowledge stacks
// ---------------------------------------------------------------------------

describe("buildKnowledgeHits", () => {
  it("builds one hit per stack/source pair and de-duplicates repeats", () => {
    const hits = [
      { stackId: "s1", stackName: "Docs", sourcePath: "readme.md", text: "the needle passage" },
      { stackId: "s1", stackName: "Docs", sourcePath: "readme.md", text: "the needle passage again" },
    ];
    const result = buildKnowledgeHits(hits, "needle");
    expect(result.hits).toHaveLength(1);
    expect(result.excludedCount).toBe(0);
    expect(result.hits[0].title).toBe("Docs — readme.md");
  });
});

// ---------------------------------------------------------------------------
// Browser workbench evidence
// ---------------------------------------------------------------------------

describe("buildBrowserEvidenceHits", () => {
  const roots = [{ path: "/Users/dev/project" }];

  it("matches evidence summaries for accessible sessions", () => {
    const session = makeSession("s1", { workspacePath: "/Users/dev/project" });
    const map = new Map([[session.id, session]]);
    const bySession = { s1: evidence("ev-1", "screenshot shows a widget error banner") };
    const result = buildBrowserEvidenceHits(bySession, map, "widget", roots);
    expect(result.hits).toHaveLength(1);
    expect(result.hits[0].sessionId).toBe("s1");
  });

  it("drops evidence for a session outside the attached workspace roots", () => {
    const session = makeSession("s1", { workspacePath: "/Users/dev/other" });
    const map = new Map([[session.id, session]]);
    const bySession = { s1: evidence("ev-1", "widget error banner") };
    const result = buildBrowserEvidenceHits(bySession, map, "widget", roots);
    expect(result.hits).toEqual([]);
    expect(result.excludedCount).toBe(1);
  });

  it("drops evidence whose owning session no longer exists", () => {
    const bySession = { "missing-session": evidence("ev-1", "widget error banner") };
    const result = buildBrowserEvidenceHits(bySession, new Map(), "widget", roots);
    expect(result.hits).toEqual([]);
    expect(result.excludedCount).toBe(1);
  });
});

// ---------------------------------------------------------------------------
// Connected apps (MCP servers)
// ---------------------------------------------------------------------------

describe("buildConnectedAppHits", () => {
  function server(overrides: Partial<McpServerLike> = {}): McpServerLike {
    return {
      id: "srv-1",
      label: "GitHub",
      enabled: true,
      status: "connected",
      tools: [{ name: "search_issues", description: "Search issues by query" }],
      ...overrides,
    };
  }

  it("matches by server label or tool name/description", () => {
    expect(buildConnectedAppHits([server({ label: "GitHub" })], "github").hits).toHaveLength(1);
    expect(buildConnectedAppHits([server()], "search_issues").hits).toHaveLength(1);
  });

  it("drops (and counts) a matching server that isn't connected", () => {
    const result = buildConnectedAppHits([server({ status: "disconnected" })], "github");
    expect(result.hits).toEqual([]);
    expect(result.excludedCount).toBe(1);
  });

  it("drops (and counts) a matching server that is disabled", () => {
    const result = buildConnectedAppHits([server({ enabled: false })], "github");
    expect(result.hits).toEqual([]);
    expect(result.excludedCount).toBe(1);
  });

  it("never reports an exclusion for a server that simply didn't match", () => {
    const result = buildConnectedAppHits([server({ status: "disconnected" })], "no-match-at-all");
    expect(result.hits).toEqual([]);
    expect(result.excludedCount).toBe(0);
  });
});

// ---------------------------------------------------------------------------
// Combine
// ---------------------------------------------------------------------------

describe("combineUniversalSearchResults", () => {
  it("flattens hits and sums excluded counts", () => {
    const combined = combineUniversalSearchResults([
      { hits: [{ id: "a" } as never], excludedCount: 2 },
      { hits: [{ id: "b" } as never], excludedCount: 1 },
    ]);
    expect(combined.hits.map((hit) => hit.id)).toEqual(["a", "b"]);
    expect(combined.excludedCount).toBe(3);
  });
});
