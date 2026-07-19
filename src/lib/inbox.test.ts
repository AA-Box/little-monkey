import { describe, expect, it } from "vitest";

import {
  buildAutomationInboxItem,
  buildAutomationInboxItems,
  buildChatApprovalInboxItem,
  buildChatApprovalInboxItems,
  buildRunInboxItem,
  buildRunInboxItems,
  buildSideTaskInboxItem,
  buildSideTaskInboxItems,
  connectorIdFromToolName,
  costBucketOf,
  deriveRunEnrichment,
  filterInboxItems,
  inboxFilterOptions,
  mergeInboxItems,
  needsApprovalCount,
  runStatusToInboxStatus,
  sideTaskStatusToInboxStatus,
  sortInboxItems,
  EMPTY_INBOX_FILTERS,
  type InboxItem,
} from "./inbox";
import type {
  ClientIdentityWire,
  ModelCapabilitiesSnapshotWire,
  ModelTargetSnapshotWire,
  RunEventEnvelopeWire,
  RunRecord,
  RunSpecWire,
  RunStatus,
  WorkspaceContextWire,
} from "./runProtocol";
import type { AutomationEntry } from "../store/automationsStore";
import type { PermissionRequest } from "../store/permissionStore";
import type { SideTaskRecord, SideTaskStatus } from "../store/sideTaskStore";

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

function makeRun(overrides: Partial<RunSpecWire> = {}, status: RunStatus = "running", extra: Partial<Omit<RunRecord, "spec" | "status">> = {}): RunRecord {
  const spec: RunSpecWire = {
    schema_version: 1,
    run_id: "run-1",
    idempotency_key: "run-1",
    created_at_ms: 1_000,
    kind: "interactive",
    submitted_by: identity(),
    task: "Fix the flaky test",
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

function envelope(event: RunEventEnvelopeWire["event"], overrides: Partial<RunEventEnvelopeWire> = {}): RunEventEnvelopeWire {
  return {
    schema_version: 1,
    event_id: `evt-${Math.random()}`,
    run_id: "run-1",
    sequence: 1,
    occurred_at_ms: 1_000,
    actor_id: null,
    emitter: identity(),
    event,
    ...overrides,
  };
}

describe("runStatusToInboxStatus", () => {
  it("maps every RunStatus to the expected bucket", () => {
    const cases: Array<[RunStatus, ReturnType<typeof runStatusToInboxStatus>]> = [
      ["queued", "active"],
      ["running", "active"],
      ["cancelling", "active"],
      ["waiting_for_permission", "waiting"],
      ["paused", "waiting"],
      ["failed", "failed"],
      ["needs_reconciliation", "failed"],
      ["cancelled", "cancelled"],
      ["succeeded", "completed"],
    ];
    for (const [status, expected] of cases) {
      expect(runStatusToInboxStatus(makeRun({}, status))).toBe(expected);
    }
  });

  it("archived overrides the underlying status", () => {
    const run = makeRun({}, "succeeded", { archivedAtMs: 5_000 });
    expect(runStatusToInboxStatus(run)).toBe("archived");
  });
});

describe("connectorIdFromToolName", () => {
  it("parses the colon-separated permission-request format", () => {
    expect(connectorIdFromToolName("mcp:github:search_issues", [])).toBe("github");
  });

  it("parses the double-underscore run-ledger format only when the id is a known server", () => {
    expect(connectorIdFromToolName("mcp__github__search_issues", ["github"])).toBe("github");
    // Sanitized/collided composite names aren't reliably reversible (see
    // mcpTools.ts) — an id that doesn't match a configured server is
    // dropped rather than guessed.
    expect(connectorIdFromToolName("mcp__github__search_issues", ["other-server"])).toBeNull();
  });

  it("returns null for non-MCP tool names", () => {
    expect(connectorIdFromToolName("write_file", ["github"])).toBeNull();
  });
});

describe("costBucketOf", () => {
  it("buckets by micros threshold", () => {
    expect(costBucketOf(null)).toBe("unknown");
    expect(costBucketOf(0)).toBe("free");
    expect(costBucketOf(499_999)).toBe("under_0_50");
    expect(costBucketOf(1_999_999)).toBe("under_2");
    expect(costBucketOf(2_000_000)).toBe("2_plus");
  });
});

describe("deriveRunEnrichment", () => {
  it("pairs tool_proposed/tool_started/tool_finished by tool_call_id and collects connectors", () => {
    const events: RunEventEnvelopeWire[] = [
      envelope({ type: "tool_proposed", payload: { tool_call_id: "t1", tool_name: "mcp__github__search_issues", arguments: { value: {}, redaction: "not_needed" }, arguments_sha256: "abc", mutation: false } }),
      envelope({ type: "tool_started", payload: { tool_call_id: "t1" } }),
      envelope({ type: "tool_finished", payload: { tool_call_id: "t1", outcome: "succeeded", output_excerpt: "two matching issues", output_sha256: "def", duration_ms: 120 } }),
    ];
    const enrichment = deriveRunEnrichment(events, ["github"]);
    expect(enrichment.toolCalls).toHaveLength(1);
    expect(enrichment.toolCalls[0]).toMatchObject({ toolCallId: "t1", started: true, outcome: "succeeded", outputExcerpt: "two matching issues", durationMs: 120, connectorId: "github" });
    expect(enrichment.connectors).toEqual(["github"]);
  });

  it("exposes the pending approval and folds decided ones into approvals history", () => {
    const events: RunEventEnvelopeWire[] = [
      envelope({ type: "permission_requested", payload: { request_id: "r1", tool_call_id: "t1", tool_name: "run_shell", operation_sha256: "sha1", expires_at_ms: 9_999, detail: "rm -rf /tmp/x", risk_level: "high", risk_reason: "deletes files" } }),
      envelope({ type: "permission_requested", payload: { request_id: "r2", tool_call_id: "t2", tool_name: "write_file", operation_sha256: "sha2", expires_at_ms: 9_999, detail: "write foo.txt", risk_level: "low", risk_reason: null } }),
      envelope({ type: "permission_decided", payload: { request_id: "r2", operation_sha256: "sha2", decision: "allow_once", decided_by: identity() } }),
    ];
    const enrichment = deriveRunEnrichment(events, []);
    expect(enrichment.approvals).toHaveLength(2);
    expect(enrichment.pendingApproval?.requestId).toBe("r1");
    expect(enrichment.pendingApproval?.riskLevel).toBe("high");
    const decided = enrichment.approvals.find((a) => a.requestId === "r2");
    expect(decided?.decision).toBe("allow_once");
  });

  it("pairs external_mutation_prepared/confirmed by mutation_id", () => {
    const events: RunEventEnvelopeWire[] = [
      envelope({ type: "external_mutation_prepared", payload: { mutation_id: "m1", tool_call_id: "t1", kind: "git", idempotency_key: null, summary: "git push" } }),
      envelope({ type: "external_mutation_confirmed", payload: { mutation_id: "m1", confirmation_ref: "sha-abc", summary: "pushed" } }),
    ];
    const enrichment = deriveRunEnrichment(events, []);
    expect(enrichment.mutations).toHaveLength(1);
    expect(enrichment.mutations[0].confirmedAtMs).not.toBeNull();
    expect(enrichment.mutations[0].confirmationRef).toBe("sha-abc");
  });

  it("collects artifacts and verification results", () => {
    const events: RunEventEnvelopeWire[] = [
      envelope({ type: "artifact_added", payload: { artifact_id: "a1", kind: "file", name: "report.md", media_type: "text/markdown", content_sha256: "sha", size_bytes: 42 } }),
      envelope({ type: "verification_finished", payload: { verification_id: "v1", name: "tsc", passed: true, summary: "0 errors", artifact_ids: [], duration_ms: 500 } }),
    ];
    const enrichment = deriveRunEnrichment(events, []);
    expect(enrichment.artifacts).toHaveLength(1);
    expect(enrichment.verifications[0]).toMatchObject({ name: "tsc", passed: true });
  });

  it("takes the latest cost/usage snapshot from usage_recorded or completed", () => {
    const events: RunEventEnvelopeWire[] = [
      envelope({ type: "usage_recorded", payload: { usage: { input_tokens: 10, output_tokens: 5, cached_input_tokens: 0, model_calls: 1, tool_calls: 0, cost_micros: 100 } } }),
      envelope({ type: "completed", payload: { summary: null, result_artifact_ids: [], usage: { input_tokens: 20, output_tokens: 15, cached_input_tokens: 0, model_calls: 2, tool_calls: 1, cost_micros: 300 } } }),
    ];
    const enrichment = deriveRunEnrichment(events, []);
    expect(enrichment.costMicros).toBe(300);
    expect(enrichment.usage?.model_calls).toBe(2);
  });

  it("returns empty-but-defined enrichment for a run with no events", () => {
    const enrichment = deriveRunEnrichment([], []);
    expect(enrichment.toolCalls).toEqual([]);
    expect(enrichment.approvals).toEqual([]);
    expect(enrichment.pendingApproval).toBeNull();
    expect(enrichment.costMicros).toBeNull();
  });
});

describe("buildRunInboxItem", () => {
  it("derives workspace/model/sourceTrigger/submittedBy straight off the real spec fields", () => {
    const run = makeRun({ kind: "workflow", submitted_by: identity("daemon"), target: target("gpt-4.1"), workspace: workspace("/Users/dev/app") });
    const item = buildRunInboxItem(run, null, []);
    expect(item.sourceKind).toBe("run");
    expect(item.sourceTrigger).toBe("workflow");
    expect(item.submittedBy).toBe("daemon");
    expect(item.model).toBe("gpt-4.1");
    expect(item.workspaceLabel).toBe("/Users/dev/app");
    expect(item.connectors).toBeNull(); // no enrichment loaded yet
  });

  it("flags needsApproval only when the run is actually waiting_for_permission", () => {
    const waiting = buildRunInboxItem(makeRun({}, "waiting_for_permission"), null, []);
    expect(waiting.needsApproval).toBe(true);
    const paused = buildRunInboxItem(makeRun({}, "paused"), null, []);
    expect(paused.needsApproval).toBe(false); // "waiting" bucket, but not an approval wait
  });

  it("marks daemon-managed runs from the real managedRunIds list", () => {
    const run = makeRun({ run_id: "run-daemon" });
    const item = buildRunInboxItem(run, null, ["run-daemon", "run-other"]);
    expect(item.daemonManaged).toBe(true);
  });

  it("surfaces enrichment's cost/connectors/risk once loaded", () => {
    const run = makeRun({}, "waiting_for_permission");
    const enrichment = deriveRunEnrichment(
      [envelope({ type: "permission_requested", payload: { request_id: "r1", tool_call_id: "t1", tool_name: "mcp:docs:search", operation_sha256: "sha", expires_at_ms: 9_999, detail: "search docs", risk_level: "medium", risk_reason: null } })],
      [],
    );
    const item = buildRunInboxItem(run, enrichment, []);
    expect(item.riskLevel).toBe("medium");
    expect(item.approvalRequestId).toBe("r1");
    expect(item.connectors).toEqual(["docs"]);
  });

  it("buildRunInboxItems maps a whole list, looking up each run's own enrichment by id", () => {
    const runs = [makeRun({ run_id: "run-a" }), makeRun({ run_id: "run-b" })];
    const enrichmentByRunId = new Map([["run-b", deriveRunEnrichment([], [])]]);
    const items = buildRunInboxItems(runs, enrichmentByRunId, []);
    expect(items.map((i) => i.runId)).toEqual(["run-a", "run-b"]);
    expect(items[0].connectors).toBeNull();
    expect(items[1].connectors).toEqual([]);
  });
});

function makeAutomation(overrides: Partial<AutomationEntry> = {}): AutomationEntry {
  return { id: "auto-1", recipeName: "nightly-cleanup", cron: "0 3 * * *", enabled: true, catchUpIfMissed: false, ...overrides };
}

describe("buildAutomationInboxItem / buildAutomationInboxItems", () => {
  it("builds a scheduled item with cron + status in the subtitle", () => {
    const item = buildAutomationInboxItem(makeAutomation({ lastStatus: "error" }), 5_000);
    expect(item.status).toBe("scheduled");
    expect(item.sourceTrigger).toBe("scheduled");
    expect(item.subtitle).toContain("0 3 * * *");
    expect(item.subtitle).toContain("last run failed");
    expect(item.nextRunAtMs).toBe(5_000);
  });

  it("filters out disabled entries entirely", () => {
    const items = buildAutomationInboxItems([makeAutomation({ id: "on", enabled: true }), makeAutomation({ id: "off", enabled: false })]);
    expect(items).toHaveLength(1);
    expect(items[0].automationEntryId).toBe("on");
  });

  it("passes through a per-entry nextRunAtMs map, defaulting to null when absent", () => {
    const items = buildAutomationInboxItems(
      [makeAutomation({ id: "a" }), makeAutomation({ id: "b" })],
      new Map([["a", 12_345]]),
    );
    expect(items.find((i) => i.automationEntryId === "a")?.nextRunAtMs).toBe(12_345);
    expect(items.find((i) => i.automationEntryId === "b")?.nextRunAtMs).toBeNull();
  });
});

function makePermissionRequest(overrides: Partial<PermissionRequest> = {}): PermissionRequest {
  return { id: "req-1", tool: "write_file", detail: "write foo.txt\n{}", ...overrides };
}

describe("buildChatApprovalInboxItem / buildChatApprovalInboxItems", () => {
  it("always needs approval and lands in the waiting bucket", () => {
    const item = buildChatApprovalInboxItem(makePermissionRequest(), []);
    expect(item.status).toBe("waiting");
    expect(item.needsApproval).toBe(true);
    expect(item.sourceKind).toBe("chat_approval");
  });

  it("parses the connector out of an mcp: tool string", () => {
    const item = buildChatApprovalInboxItem(makePermissionRequest({ tool: "mcp:github:search_issues" }), []);
    expect(item.connectors).toEqual(["github"]);
  });

  it("folds the subagent attribution into the title when present", () => {
    const item = buildChatApprovalInboxItem(makePermissionRequest({ agent_label: "code-review subagent" }), []);
    expect(item.title).toContain("code-review subagent");
  });

  it("carries the risk level straight through", () => {
    const item = buildChatApprovalInboxItem(makePermissionRequest({ risk_level: "high" }), []);
    expect(item.riskLevel).toBe("high");
  });

  it("builds one item per queued request, preserving order", () => {
    const items = buildChatApprovalInboxItems([makePermissionRequest({ id: "a" }), makePermissionRequest({ id: "b" })], []);
    expect(items.map((i) => i.approvalRequestId)).toEqual(["a", "b"]);
  });
});

function makeSideTask(status: SideTaskStatus, overrides: Partial<SideTaskRecord> = {}): SideTaskRecord {
  return {
    id: `task-${status}`,
    turnId: `turn-${status}`,
    retryOf: null,
    title: "Investigate auth",
    prompt: "Investigate auth",
    profile: "explore",
    status,
    source: { kind: "terminal_output", label: "Terminal evidence · app", excerpt: "failure" },
    sessionId: "session-1",
    modelLabel: "local · qwen",
    createdAt: 1_000,
    updatedAt: 2_000,
    startedAt: status === "queued" ? null : 1_100,
    finishedAt: ["completed", "error", "cancelled"].includes(status) ? 2_000 : null,
    messages: [{ role: "user", content: "Investigate auth" }],
    toolEvidence: [],
    artifacts: [],
    usage: null,
    error: status === "error" ? "failed" : null,
    finalReport: status === "completed" ? "done" : null,
    promotedAt: null,
    archivedAt: null,
    ...overrides,
  };
}

describe("buildSideTaskInboxItem / buildSideTaskInboxItems", () => {
  it("maps every live side-task status and lets archive override it", () => {
    const cases: Array<[SideTaskStatus, InboxItem["status"]]> = [
      ["queued", "waiting"],
      ["running", "active"],
      ["paused", "waiting"],
      ["completed", "completed"],
      ["error", "failed"],
      ["cancelled", "cancelled"],
    ];
    for (const [status, expected] of cases) {
      expect(sideTaskStatusToInboxStatus(makeSideTask(status))).toBe(expected);
    }
    expect(sideTaskStatusToInboxStatus(makeSideTask("completed", { archivedAt: 3_000 }))).toBe("archived");
  });

  it("preserves source, model, identity, timestamps, and archive state", () => {
    const task = makeSideTask("completed", { archivedAt: 3_000 });
    const item = buildSideTaskInboxItem(task);
    expect(item).toMatchObject({
      id: "side-task:task-completed",
      sideTaskId: "task-completed",
      sourceKind: "side_task",
      sourceTrigger: "terminal_output",
      status: "archived",
      model: "local · qwen",
      createdAtMs: 1_000,
      updatedAtMs: 2_000,
      archivedAtMs: 3_000,
    });
  });

  it("maps a whole store snapshot without losing insertion order", () => {
    const tasks = [makeSideTask("running"), makeSideTask("error")];
    expect(buildSideTaskInboxItems(tasks).map((item) => item.sideTaskId)).toEqual(["task-running", "task-error"]);
  });
});

describe("mergeInboxItems / sortInboxItems", () => {
  it("orders by status priority (waiting, active, failed, scheduled, completed, cancelled, archived) then recency", () => {
    const base = (id: string, status: InboxItem["status"], updatedAtMs: number): InboxItem => ({
      id, sourceKind: "run", status, title: id, subtitle: "", createdAtMs: updatedAtMs, updatedAtMs,
      workspaceId: null, workspaceLabel: null, sourceTrigger: "interactive", submittedBy: "desktop",
      model: null, connectors: null, costMicros: null, riskLevel: null, needsApproval: false,
      runId: id, automationEntryId: null, approvalRequestId: null, sideTaskId: null, archivedAtMs: null, daemonManaged: false, nextRunAtMs: null,
    });
    const items = mergeInboxItems(
      [base("completed-old", "completed", 100), base("archived", "archived", 900)],
      [base("waiting", "waiting", 50)],
      [base("active", "active", 200), base("failed", "failed", 300)],
      [base("cancelled", "cancelled", 400), base("scheduled", "scheduled", 250)],
    );
    const sorted = sortInboxItems(items);
    expect(sorted.map((i) => i.id)).toEqual([
      "waiting", "active", "failed", "scheduled", "completed-old", "cancelled", "archived",
    ]);
  });

  it("sorts by updatedAtMs descending within the same status bucket", () => {
    const base = (id: string, updatedAtMs: number): InboxItem => ({
      id, sourceKind: "run", status: "active", title: id, subtitle: "", createdAtMs: updatedAtMs, updatedAtMs,
      workspaceId: null, workspaceLabel: null, sourceTrigger: "interactive", submittedBy: "desktop",
      model: null, connectors: null, costMicros: null, riskLevel: null, needsApproval: false,
      runId: id, automationEntryId: null, approvalRequestId: null, sideTaskId: null, archivedAtMs: null, daemonManaged: false, nextRunAtMs: null,
    });
    const sorted = sortInboxItems([base("older", 100), base("newer", 200)]);
    expect(sorted.map((i) => i.id)).toEqual(["newer", "older"]);
  });
});

describe("filterInboxItems / inboxFilterOptions / needsApprovalCount", () => {
  function items(): InboxItem[] {
    const runA = buildRunInboxItem(
      makeRun({ run_id: "run-a", kind: "interactive", target: target("qwen2.5:14b"), workspace: workspace("/ws/a") }, "waiting_for_permission"),
      deriveRunEnrichment(
        [envelope({ type: "permission_requested", payload: { request_id: "r1", tool_call_id: "t1", tool_name: "mcp:github:search", operation_sha256: "s", expires_at_ms: 1, detail: "d", risk_level: "high", risk_reason: null } })],
        ["github"],
      ),
      [],
    );
    const runB = buildRunInboxItem(
      makeRun({ run_id: "run-b", kind: "workflow", task: "Summarize the changelog", target: target("gpt-4.1"), workspace: workspace("/ws/b") }, "succeeded"),
      deriveRunEnrichment([envelope({ type: "usage_recorded", payload: { usage: { input_tokens: 1, output_tokens: 1, cached_input_tokens: 0, model_calls: 1, tool_calls: 0, cost_micros: 2_500_000 } } })], []),
      [],
    );
    const automation = buildAutomationInboxItem(makeAutomation(), null);
    return [runA, runB, automation];
  }

  it("search matches title or subtitle case-insensitively", () => {
    const result = filterInboxItems(items(), { ...EMPTY_INBOX_FILTERS, search: "flaky" });
    expect(result.map((i) => i.id)).toEqual(["run:run-a"]);
  });

  it("filters by workspaceId", () => {
    const result = filterInboxItems(items(), { ...EMPTY_INBOX_FILTERS, workspaceId: "workspace-/ws/b" });
    expect(result.map((i) => i.id)).toEqual(["run:run-b"]);
  });

  it("filters by sourceTrigger", () => {
    const result = filterInboxItems(items(), { ...EMPTY_INBOX_FILTERS, sourceTrigger: "workflow" });
    expect(result.map((i) => i.id)).toEqual(["run:run-b"]);
  });

  it("filters by model", () => {
    const result = filterInboxItems(items(), { ...EMPTY_INBOX_FILTERS, model: "qwen2.5:14b" });
    expect(result.map((i) => i.id)).toEqual(["run:run-a"]);
  });

  it("filters by connector", () => {
    const result = filterInboxItems(items(), { ...EMPTY_INBOX_FILTERS, connector: "github" });
    expect(result.map((i) => i.id)).toEqual(["run:run-a"]);
  });

  it("filters by status", () => {
    const result = filterInboxItems(items(), { ...EMPTY_INBOX_FILTERS, status: "scheduled" });
    expect(result.map((i) => i.id)).toEqual(["automation:auto-1"]);
  });

  it("filters by cost bucket", () => {
    const result = filterInboxItems(items(), { ...EMPTY_INBOX_FILTERS, costBucket: "2_plus" });
    expect(result.map((i) => i.id)).toEqual(["run:run-b"]);
  });

  it("filters by risk level, including the synthetic 'unknown' bucket", () => {
    expect(filterInboxItems(items(), { ...EMPTY_INBOX_FILTERS, riskLevel: "high" }).map((i) => i.id)).toEqual(["run:run-a"]);
    const unknownRisk = filterInboxItems(items(), { ...EMPTY_INBOX_FILTERS, riskLevel: "unknown" }).map((i) => i.id);
    expect(unknownRisk).toEqual(["run:run-b", "automation:auto-1"]);
  });

  it("combines multiple filters with AND semantics", () => {
    const result = filterInboxItems(items(), { ...EMPTY_INBOX_FILTERS, sourceTrigger: "workflow", costBucket: "unknown" });
    expect(result).toEqual([]);
  });

  it("derives dropdown options only from what's actually present in the data", () => {
    const options = inboxFilterOptions(items());
    expect(options.workspaces.map((w) => w.id).sort()).toEqual(["workspace-/ws/a", "workspace-/ws/b"]);
    expect(options.sourceTriggers).toEqual(["interactive", "scheduled", "workflow"]);
    expect(options.models).toEqual(["gpt-4.1", "qwen2.5:14b"]);
    expect(options.connectors).toEqual(["github"]);
  });

  it("counts items that need the user's approval across every source", () => {
    expect(needsApprovalCount(items())).toBe(1);
  });
});
