import { describe, expect, it } from "vitest";

import {
  buildRunCapsule,
  compareRunCapsules,
  createRedactedRunCapsuleExport,
  runCapsuleFileName,
} from "./runCapsule";
import {
  EMPTY_USAGE,
  type ClientIdentityWire,
  type ModelCapabilitiesSnapshotWire,
  type RunEventEnvelopeWire,
  type RunEventWire,
  type RunRecord,
  type RunStatus,
  type UsageSnapshotWire,
} from "./runProtocol";

const UNKNOWN_CAPABILITY = { state: "unknown" as const, evidence: "not assessed in tests" };
const CAPABILITIES: ModelCapabilitiesSnapshotWire = {
  tool_calling: UNKNOWN_CAPABILITY,
  vision: UNKNOWN_CAPABILITY,
  embeddings: UNKNOWN_CAPABILITY,
  structured_output: UNKNOWN_CAPABILITY,
  image_generation: UNKNOWN_CAPABILITY,
  audio: UNKNOWN_CAPABILITY,
  runtime_lifecycle: UNKNOWN_CAPABILITY,
  fim: UNKNOWN_CAPABILITY,
  code_completion: UNKNOWN_CAPABILITY,
  inline_edit: UNKNOWN_CAPABILITY,
  fim_metadata: null,
};
const EMITTER: ClientIdentityWire = { client_id: "test", instance_id: "test", kind: "test", version: "0" };
const WORKSPACE_ROOT = "/Users/tester/projects/demo";
const CREATED_AT_MS = 1_000;
const SHA = "f".repeat(64);

function makeRun(overrides: { runId?: string; status?: RunStatus; label?: string; task?: string } = {}): RunRecord {
  const runId = overrides.runId ?? "run-1";
  return {
    spec: {
      schema_version: 1,
      run_id: runId,
      idempotency_key: `test/${runId}`,
      created_at_ms: CREATED_AT_MS,
      kind: "interactive",
      submitted_by: EMITTER,
      task: overrides.task ?? "Summarize the demo project",
      instructions: null,
      input_artifact_ids: [],
      target: {
        kind: "ollama",
        target_id: "target-demo",
        label: overrides.label ?? "Ollama · llama3",
        base_url: "http://127.0.0.1:11434",
        model: "llama3",
        is_cloud: false,
        capabilities: CAPABILITIES,
        estimated_memory_bytes: null,
      },
      workspace: {
        workspace_id: "workspace-demo",
        primary_root_id: "root-1",
        roots: [{ root_id: "root-1", canonical_path: WORKSPACE_ROOT, access: "read_write", allow_symlinks_within_root: false }],
        repository_policy: null,
      },
      permission_policy: {
        mode: "manual",
        unattended: false,
        approval_timeout_ms: 300_000,
        default_tool_decision: "prompt",
        tool_rules: [],
        allow_network: false,
        allow_external_mutations: false,
      },
      budgets: {
        wall_time_ms: 60_000,
        max_iterations: 8,
        max_model_calls: 8,
        max_tool_calls: 16,
        max_input_tokens: 100_000,
        max_output_tokens: 50_000,
        max_cost_micros: null,
        max_artifact_bytes: 1_048_576,
        max_event_count: 1_000,
      },
    },
    status: overrides.status ?? "succeeded",
    lastSequence: 0,
    terminalSequence: null,
    updatedAtMs: CREATED_AT_MS,
    archivedAtMs: null,
  };
}

function envelope(sequence: number, event: RunEventWire): RunEventEnvelopeWire {
  return {
    schema_version: 1,
    event_id: `event-${sequence}`,
    run_id: "run-1",
    sequence,
    occurred_at_ms: CREATED_AT_MS + sequence * 100,
    actor_id: null,
    emitter: EMITTER,
    event,
  };
}

function toolProposed(
  sequence: number,
  toolCallId: string,
  toolName: string,
  mutation = false,
  value: unknown = {},
): RunEventEnvelopeWire {
  return envelope(sequence, {
    type: "tool_proposed",
    payload: {
      tool_call_id: toolCallId,
      tool_name: toolName,
      arguments: { value, redaction: "not_needed" },
      arguments_sha256: SHA,
      mutation,
    },
  });
}

function toolFinished(sequence: number, toolCallId: string, outcome: "succeeded" | "failed" = "succeeded"): RunEventEnvelopeWire {
  return envelope(sequence, {
    type: "tool_finished",
    payload: { tool_call_id: toolCallId, outcome, output_excerpt: "ok", output_sha256: SHA, duration_ms: 42 },
  });
}

function usage(overrides: Partial<UsageSnapshotWire> = {}): UsageSnapshotWire {
  return { ...EMPTY_USAGE, ...overrides };
}

function completed(sequence: number, snapshot: UsageSnapshotWire = usage()): RunEventEnvelopeWire {
  return envelope(sequence, { type: "completed", payload: { summary: null, result_artifact_ids: [], usage: snapshot } });
}

describe("buildRunCapsule", () => {
  it("populates every evidence section from the durable event stream", () => {
    const events = [
      envelope(1, { type: "queued", payload: { queue: "desktop-interactive" } }),
      envelope(2, { type: "started", payload: { engine_id: "engine-v1" } }),
      toolProposed(3, "tool-1", "write_file", true, { path: `${WORKSPACE_ROOT}/notes.md`, content: "hello" }),
      envelope(4, {
        type: "permission_requested",
        payload: {
          request_id: "request-1",
          tool_call_id: "tool-1",
          tool_name: "write_file",
          operation_sha256: SHA,
          expires_at_ms: CREATED_AT_MS + 60_000,
          detail: "Write notes.md",
          risk_level: "medium",
          risk_reason: "Writes into the workspace",
        },
      }),
      envelope(5, {
        type: "permission_decided",
        payload: { request_id: "request-1", operation_sha256: SHA, decision: "allow_once", decided_by: EMITTER },
      }),
      envelope(6, { type: "tool_started", payload: { tool_call_id: "tool-1" } }),
      toolFinished(7, "tool-1"),
      envelope(8, {
        type: "artifact_added",
        payload: { artifact_id: SHA, kind: "image", name: "browser_screenshot: capture", media_type: "image/png", content_sha256: SHA, size_bytes: 2_048 },
      }),
      envelope(9, {
        type: "verification_finished",
        payload: { verification_id: "verify-1", name: "vitest", passed: true, summary: "All tests passed", artifact_ids: [], duration_ms: 900 },
      }),
      envelope(10, {
        type: "external_mutation_confirmed",
        payload: { mutation_id: "mutation-1", confirmation_ref: "issue-77", summary: "Created issue in tracker" },
      }),
      completed(11, usage({ input_tokens: 120, output_tokens: 30, model_calls: 2, tool_calls: 1, cost_micros: 5_000 })),
    ];

    const capsule = buildRunCapsule(makeRun(), events);

    expect(capsule.tools).toHaveLength(1);
    expect(capsule.tools[0]).toMatchObject({ name: "write_file", mutation: true, outcome: "succeeded", durationMs: 42, outputExcerpt: "ok" });
    expect(capsule.approvals).toEqual([
      expect.objectContaining({ requestId: "request-1", toolName: "write_file", decision: "allow_once", requestedSequence: 4, decidedSequence: 5 }),
    ]);
    expect(capsule.fileChanges).toEqual([
      expect.objectContaining({ path: `${WORKSPACE_ROOT}/notes.md`, toolName: "write_file", outcome: "succeeded" }),
    ]);
    expect(capsule.artifacts).toEqual([
      expect.objectContaining({ artifactId: SHA, kind: "image", browserEvidence: true, sizeBytes: 2_048 }),
    ]);
    expect(capsule.verifications).toEqual([expect.objectContaining({ name: "vitest", passed: true })]);
    expect(capsule.replay.classification).toBe("non_repeatable");
    expect(capsule.replay.externalEffects).toContain("Created issue in tracker");
    expect(capsule.usage).toMatchObject({ input_tokens: 120, output_tokens: 30, cost_micros: 5_000 });
    expect(capsule.run.durationMs).toBe(events[events.length - 1].occurred_at_ms - CREATED_AT_MS);
    expect(capsule.timeline.map((entry) => entry.sequence)).toEqual(events.map((entry) => entry.sequence));
    expect(capsule.limitations.some((entry) => entry.includes("cost"))).toBe(false);
  });

  it("classifies a terminal run without model output or mutations as deterministic", () => {
    const events = [
      envelope(1, { type: "started", payload: { engine_id: "engine-v1" } }),
      toolProposed(2, "tool-1", "read_file", false, { path: "notes.md" }),
      toolFinished(3, "tool-1"),
      completed(4),
    ];
    const capsule = buildRunCapsule(makeRun(), events);
    expect(capsule.replay).toMatchObject({ classification: "deterministic", boundary: "from_start", safeFromStart: true });
    expect(capsule.limitations.some((entry) => entry.includes("cost"))).toBe(true);
  });

  it("downgrades model-observed runs to best-effort with a fresh-run boundary", () => {
    const events = [
      envelope(1, { type: "started", payload: { engine_id: "engine-v1" } }),
      envelope(2, { type: "model_delta", payload: { message_id: "m1", channel: "assistant", text: "answer" } }),
      completed(3, usage({ input_tokens: 10, output_tokens: 5, model_calls: 1 })),
    ];
    const capsule = buildRunCapsule(makeRun(), events);
    expect(capsule.replay).toMatchObject({ classification: "best_effort", boundary: "fresh_run_from_frozen_spec", safeFromStart: true });
  });

  it("keeps runs that crossed a local mutation boundary inspectable only", () => {
    const events = [
      envelope(1, { type: "model_delta", payload: { message_id: "m1", channel: "assistant", text: "editing" } }),
      toolProposed(2, "tool-1", "write_file", true, { path: "notes.md", content: "x" }),
      toolFinished(3, "tool-1"),
      completed(4, usage({ model_calls: 1 })),
    ];
    const capsule = buildRunCapsule(makeRun(), events);
    expect(capsule.replay).toMatchObject({ classification: "best_effort", boundary: "inspection_only", safeFromStart: false });
  });

  it("treats a completed connector mutation as a non-repeatable external effect", () => {
    const events = [
      toolProposed(1, "tool-1", "mcp__tracker__create_issue", true),
      toolFinished(2, "tool-1"),
      completed(3, usage({ tool_calls: 1 })),
    ];
    const capsule = buildRunCapsule(makeRun(), events);
    expect(capsule.connectorCalls).toEqual([expect.objectContaining({ toolName: "mcp__tracker__create_issue", mutationBoundary: true })]);
    expect(capsule.replay).toMatchObject({ classification: "non_repeatable", boundary: "inspection_only", safeFromStart: false });
    expect(capsule.replay.externalEffects).toContain("mcp__tracker__create_issue completed");
  });

  it("marks ledger-flagged reconciliation as non-repeatable", () => {
    const events = [
      envelope(1, { type: "needs_reconciliation", payload: { mutation_id: "mutation-1", reason: "unknown external outcome" } }),
    ];
    const capsule = buildRunCapsule(makeRun({ status: "needs_reconciliation" }), events);
    expect(capsule.replay).toMatchObject({ classification: "non_repeatable", boundary: "inspection_only", safeFromStart: false });
  });

  it("never offers a safe boundary while the run is still active", () => {
    const events = [
      envelope(1, { type: "started", payload: { engine_id: "engine-v1" } }),
      envelope(2, { type: "model_delta", payload: { message_id: "m1", channel: "assistant", text: "working" } }),
    ];
    const capsule = buildRunCapsule(makeRun({ status: "running" }), events);
    expect(capsule.replay.safeFromStart).toBe(false);
    expect(capsule.replay.reasons.some((reason) => reason.includes("still active"))).toBe(true);
    expect(capsule.run.durationMs).toBeNull();
  });
});

describe("createRedactedRunCapsuleExport", () => {
  it("redacts secret fields, home paths, and workspace roots while counting replacements", () => {
    // Split so secret scanners don't flag the fixture as a real key.
    const fakeKey = ["sk-live-", "abcdef123456"].join("");
    const events = [
      toolProposed(1, "tool-1", "http_request", false, {
        apiKey: fakeKey,
        path: `${WORKSPACE_ROOT}/src/index.ts`,
        note: "config lives at /Users/tester/.config/app.toml",
      }),
      completed(2),
    ];
    const capsule = buildRunCapsule(makeRun(), events);
    const exported = createRedactedRunCapsuleExport(capsule, 9_999);

    const args = exported.capsule.tools[0].arguments as Record<string, string>;
    expect(args.apiKey).toBe("[REDACTED]");
    expect(args.path).toBe("$WORKSPACE_1/src/index.ts");
    expect(args.note).toBe("config lives at $HOME/.config/app.toml");
    expect(exported.exportedAtMs).toBe(9_999);
    expect(exported.redaction.applied).toBe(true);
    expect(exported.redaction.replacements).toBeGreaterThanOrEqual(3);
    const serialized = JSON.stringify(exported);
    expect(serialized).not.toContain(fakeKey);
    expect(serialized).not.toContain("/Users/tester");
  });
});

describe("compareRunCapsules", () => {
  it("reports changed metric rows and partitions the tool sets", () => {
    const left = buildRunCapsule(makeRun(), [
      toolProposed(1, "tool-1", "read_file"),
      toolProposed(2, "tool-2", "write_file", true),
      completed(3, usage({ input_tokens: 100, output_tokens: 20, model_calls: 1, tool_calls: 2 })),
    ]);
    const right = buildRunCapsule(makeRun({ runId: "run-2", label: "Ollama · qwen3", status: "failed" }), [
      toolProposed(1, "tool-1", "read_file"),
      toolProposed(2, "tool-2", "run_shell", true),
      envelope(3, { type: "failed", payload: { code: "boom", message: "engine failed", retryable: false } }),
    ]);

    const comparison = compareRunCapsules(left, right);
    expect(comparison.leftRunId).toBe("run-1");
    expect(comparison.rightRunId).toBe("run-2");
    expect(comparison.sharedTools).toEqual(["read_file"]);
    expect(comparison.leftOnlyTools).toEqual(["write_file"]);
    expect(comparison.rightOnlyTools).toEqual(["run_shell"]);
    expect(comparison.rows.find((row) => row.key === "target")).toMatchObject({ changed: true });
    expect(comparison.rows.find((row) => row.key === "status")).toMatchObject({ left: "succeeded", right: "failed", changed: true });
    expect(comparison.rows.find((row) => row.key === "kind")).toMatchObject({ changed: false });
    expect(comparison.changedFields).toBe(comparison.rows.filter((row) => row.changed).length);
  });
});

describe("runCapsuleFileName", () => {
  it("sanitizes hostile run ids into a filesystem-safe name", () => {
    const capsule = buildRunCapsule(makeRun({ runId: "run/../We ird:id?" }), []);
    const name = runCapsuleFileName(capsule);
    expect(name).toBe("little-monkey-run-..-We-ird-id-.lmcapsule.json");
    expect(name).toMatch(/^little-monkey-[A-Za-z0-9_.-]+\.lmcapsule\.json$/);
  });

  it("falls back to a stable name when the run id is empty", () => {
    const capsule = buildRunCapsule(makeRun({ runId: "" }), []);
    expect(runCapsuleFileName(capsule)).toBe("little-monkey-run.lmcapsule.json");
  });
});
