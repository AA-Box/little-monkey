import { describe, expect, it } from "vitest";

import {
  buildConnectorHighlights,
  buildDailyBrief,
  buildFailedScheduledJobs,
  buildPendingApprovals,
  buildRecentlyCompleted,
  buildRunningTasks,
  buildRuntimeHealth,
  buildStaleTasks,
  STALE_THRESHOLD_MS,
} from "./dailyBriefStore";
import type {
  ClientIdentityWire,
  ModelCapabilitiesSnapshotWire,
  ModelTargetSnapshotWire,
  RunRecord,
  RunSpecWire,
  RunStatus,
  WorkspaceContextWire,
} from "../lib/runProtocol";
import type { AutomationEntry } from "./automationsStore";
import type { PermissionRequest } from "./permissionStore";
import type { M3RuntimeCapability } from "../lib/runtimeHubClient";

// ---------------------------------------------------------------------------
// Fixtures (mirrors src/lib/inbox.test.ts's own fixture shapes)
// ---------------------------------------------------------------------------

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

function target(label = "qwen2.5:14b"): ModelTargetSnapshotWire {
  return {
    kind: "ollama",
    target_id: "target-1",
    label,
    base_url: "http://127.0.0.1:11434",
    model: "qwen2.5:14b",
    is_cloud: false,
    capabilities: CAPABILITIES,
    estimated_memory_bytes: null,
  };
}

function identity(kind: ClientIdentityWire["kind"] = "desktop"): ClientIdentityWire {
  return { client_id: "little-monkey-desktop", instance_id: "main", kind, version: "0.1.0" };
}

function workspace(path = "/Users/dev/project"): WorkspaceContextWire {
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
    workspace: workspace(),
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

function permissionRequest(overrides: Partial<PermissionRequest> = {}): PermissionRequest {
  return {
    id: "req-1",
    tool: "run_shell",
    detail: "rm -rf build/\nCleans the build directory.",
    ...overrides,
  };
}

function automationEntry(overrides: Partial<AutomationEntry> = {}): AutomationEntry {
  return {
    id: "automation-1",
    recipeName: "Nightly backup",
    cron: "0 3 * * *",
    enabled: true,
    catchUpIfMissed: false,
    ...overrides,
  };
}

function runtimeCapability(overrides: Partial<M3RuntimeCapability["descriptor"]> = {}, canInfer = true): M3RuntimeCapability {
  return {
    descriptor: {
      runtimeId: "ollama-local",
      kind: "ollama",
      label: "Ollama (local)",
      managed: false,
      apiBackend: "ollama",
      ...overrides,
    },
    canLoad: true,
    canUnload: true,
    canLogs: false,
    canMetrics: false,
    canInfer,
    settings: [],
  };
}

// ---------------------------------------------------------------------------
// Pending approvals
// ---------------------------------------------------------------------------

describe("buildPendingApprovals", () => {
  it("includes both queued chat approvals and runs waiting on permission", () => {
    const queue = [permissionRequest({ id: "req-1" }), permissionRequest({ id: "req-2", tool: "write_file" })];
    const runs = [
      makeRun("run-waiting", "waiting_for_permission", {}, { updatedAtMs: 5_000 }),
      makeRun("run-running", "running"),
    ];
    const result = buildPendingApprovals(queue, runs, 10_000);
    expect(result).toHaveLength(3);
    expect(result.filter((item) => item.kind === "chat_approval")).toHaveLength(2);
    const runApproval = result.find((item) => item.kind === "run");
    expect(runApproval?.runId).toBe("run-waiting");
    expect(runApproval?.requestedAtMs).toBe(5_000);
  });

  it("excludes archived runs even if still waiting_for_permission", () => {
    const runs = [makeRun("run-archived", "waiting_for_permission", {}, { archivedAtMs: 9_000 })];
    expect(buildPendingApprovals([], runs, 10_000)).toEqual([]);
  });

  it("takes a chat approval's detail from only its first line", () => {
    const queue = [permissionRequest({ detail: "first line\nsecond line" })];
    const result = buildPendingApprovals(queue, [], 10_000);
    expect(result[0].detail).toBe("first line");
  });

  it("sorts by most recently requested first", () => {
    const runs = [
      makeRun("older", "waiting_for_permission", {}, { updatedAtMs: 1_000 }),
      makeRun("newer", "waiting_for_permission", {}, { updatedAtMs: 9_000 }),
    ];
    const result = buildPendingApprovals([], runs, 10_000);
    expect(result.map((item) => item.runId)).toEqual(["newer", "older"]);
  });
});

// ---------------------------------------------------------------------------
// Running tasks
// ---------------------------------------------------------------------------

describe("buildRunningTasks", () => {
  it("includes queued, running, cancelling, and paused runs", () => {
    const runs = [
      makeRun("a", "queued"),
      makeRun("b", "running"),
      makeRun("c", "cancelling"),
      makeRun("d", "paused"),
    ];
    expect(buildRunningTasks(runs)).toHaveLength(4);
  });

  it("excludes waiting_for_permission (that belongs to pending approvals) and terminal statuses", () => {
    const runs = [
      makeRun("waiting", "waiting_for_permission"),
      makeRun("done", "succeeded"),
      makeRun("failed", "failed"),
      makeRun("cancelled", "cancelled"),
    ];
    expect(buildRunningTasks(runs)).toEqual([]);
  });

  it("excludes archived runs", () => {
    const runs = [makeRun("archived", "running", {}, { archivedAtMs: 1_000 })];
    expect(buildRunningTasks(runs)).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// Failed scheduled jobs
// ---------------------------------------------------------------------------

describe("buildFailedScheduledJobs", () => {
  it("includes only enabled entries whose last run failed or was denied", () => {
    const entries = [
      automationEntry({ id: "ok", lastStatus: "ok", lastRunAt: 1_000 }),
      automationEntry({ id: "error", lastStatus: "error", lastRunAt: 2_000 }),
      automationEntry({ id: "denied", lastStatus: "denied", lastRunAt: 3_000 }),
      automationEntry({ id: "disabled-error", lastStatus: "error", lastRunAt: 4_000, enabled: false }),
      automationEntry({ id: "never-run", lastStatus: undefined, lastRunAt: undefined }),
    ];
    const result = buildFailedScheduledJobs(entries);
    expect(result.map((item) => item.automationEntryId)).toEqual(["denied", "error"]);
  });
});

// ---------------------------------------------------------------------------
// Recently completed
// ---------------------------------------------------------------------------

describe("buildRecentlyCompleted", () => {
  it("only includes succeeded, non-archived runs, newest first, capped at the limit", () => {
    const runs = [
      makeRun("s1", "succeeded", {}, { updatedAtMs: 1_000 }),
      makeRun("s2", "succeeded", {}, { updatedAtMs: 3_000 }),
      makeRun("s3", "succeeded", {}, { updatedAtMs: 2_000 }),
      makeRun("failed", "failed"),
      makeRun("archived", "succeeded", {}, { archivedAtMs: 500 }),
    ];
    const result = buildRecentlyCompleted(runs, 2);
    expect(result.map((item) => item.runId)).toEqual(["s2", "s3"]);
  });
});

// ---------------------------------------------------------------------------
// Stale tasks
// ---------------------------------------------------------------------------

describe("buildStaleTasks", () => {
  const now = 100 * STALE_THRESHOLD_MS;

  it("flags in-flight runs that haven't updated within the threshold", () => {
    const runs = [
      makeRun("stale", "running", {}, { updatedAtMs: now - STALE_THRESHOLD_MS - 1 }),
      makeRun("fresh", "running", {}, { updatedAtMs: now - 1_000 }),
    ];
    const result = buildStaleTasks(runs, now);
    expect(result.map((item) => item.runId)).toEqual(["stale"]);
    expect(result[0].staleForMs).toBeGreaterThanOrEqual(STALE_THRESHOLD_MS);
  });

  it("considers a long-unanswered approval stale too", () => {
    const runs = [makeRun("stuck", "waiting_for_permission", {}, { updatedAtMs: now - STALE_THRESHOLD_MS - 1 })];
    expect(buildStaleTasks(runs, now)).toHaveLength(1);
  });

  it("never flags terminal or archived runs", () => {
    const runs = [
      makeRun("done", "succeeded", {}, { updatedAtMs: 0 }),
      makeRun("archived", "running", {}, { updatedAtMs: 0, archivedAtMs: 1 }),
    ];
    expect(buildStaleTasks(runs, now)).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// Connector highlights (intentionally always empty — see doc comment)
// ---------------------------------------------------------------------------

describe("buildConnectorHighlights", () => {
  it("returns no fabricated highlights", () => {
    expect(buildConnectorHighlights()).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// Runtime health
// ---------------------------------------------------------------------------

describe("buildRuntimeHealth", () => {
  it("reports hasData: false and empty fields before the overview has loaded", () => {
    const result = buildRuntimeHealth({ loaded: false, runtimes: [], hardware: null, storage: null, errors: {} });
    expect(result.hasData).toBe(false);
    expect(result.nodes).toEqual([]);
    expect(result.inferenceReadyCount).toBe(0);
  });

  it("summarizes configured runtimes, inference readiness, and storage once loaded", () => {
    const runtimes = [
      runtimeCapability({ runtimeId: "ollama-local", label: "Ollama" }, true),
      runtimeCapability({ runtimeId: "llama-cpp", label: "Managed llama.cpp", kind: "llama_cpp" }, false),
    ];
    const result = buildRuntimeHealth({
      loaded: true,
      runtimes,
      hardware: null,
      storage: { root: "/models", quotaBytes: 100, reserveBytes: 10, usedBytes: 40, availableForModelsBytes: 50, pendingDownloadBytes: 0 },
      errors: { overview: "network unreachable" },
    });
    expect(result.hasData).toBe(true);
    expect(result.nodes).toHaveLength(2);
    expect(result.inferenceReadyCount).toBe(1);
    expect(result.storageUsedBytes).toBe(40);
    expect(result.storageQuotaBytes).toBe(100);
    expect(result.overviewError).toBe("network unreachable");
    expect(result.lanError).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

describe("buildDailyBrief", () => {
  it("assembles every section from the given source state", () => {
    const runs = [
      makeRun("running", "running"),
      makeRun("waiting", "waiting_for_permission", {}, { updatedAtMs: 5_000 }),
      makeRun("done", "succeeded", {}, { updatedAtMs: 6_000 }),
    ];
    const result = buildDailyBrief({
      runs,
      automationEntries: [automationEntry({ lastStatus: "error", lastRunAt: 1_000 })],
      permissionQueue: [permissionRequest()],
      runtimeHub: { loaded: true, runtimes: [], hardware: null, storage: null, errors: {} },
      nowMs: 10_000,
    });
    expect(result.pendingApprovals).toHaveLength(2);
    expect(result.running).toHaveLength(1);
    expect(result.failedScheduledJobs).toHaveLength(1);
    expect(result.recentlyCompleted).toHaveLength(1);
    expect(result.connectorHighlights).toEqual([]);
    expect(result.runtimeHealth.hasData).toBe(true);
  });
});
