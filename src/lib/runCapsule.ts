import { redactPrivatePaths, redactSensitiveText } from "./durableRun";
import {
  EMPTY_USAGE,
  type ArtifactKind,
  type PermissionDecision,
  type RunEventEnvelopeWire,
  type RunRecord,
  type RunStatus,
  type ToolOutcome,
  type UsageSnapshotWire,
} from "./runProtocol";

export const RUN_CAPSULE_FORMAT = "little-monkey-run-capsule" as const;
export const RUN_CAPSULE_SCHEMA_VERSION = 1 as const;
export const RUN_CAPSULE_REDACTION_VERSION = 1 as const;

export type ReplayClassification = "deterministic" | "best_effort" | "non_repeatable";

export interface RunCapsuleTool {
  toolCallId: string;
  name: string;
  arguments: unknown;
  argumentsRedaction: "applied" | "not_needed";
  argumentsSha256: string;
  mutation: boolean;
  outcome: ToolOutcome | "not_finished";
  durationMs: number | null;
  outputExcerpt: string | null;
  proposedSequence: number;
}

export interface RunCapsuleApproval {
  requestId: string;
  toolCallId: string;
  toolName: string;
  detail: string;
  operationSha256: string;
  riskLevel: string | null;
  riskReason: string | null;
  expiresAtMs: number;
  decision: PermissionDecision | "pending";
  requestedSequence: number;
  decidedSequence: number | null;
}

export interface RunCapsuleFileChange {
  path: string;
  toolName: string;
  toolCallId: string;
  outcome: ToolOutcome;
  sequence: number;
}

export interface RunCapsuleArtifact {
  artifactId: string;
  kind: ArtifactKind;
  name: string;
  mediaType: string;
  contentSha256: string;
  sizeBytes: number;
  sequence: number;
  browserEvidence: boolean;
}

export interface RunCapsuleTerminalExcerpt {
  toolCallId: string;
  toolName: string;
  outcome: ToolOutcome | "not_finished";
  excerpt: string | null;
  durationMs: number | null;
  sequence: number;
}

export interface RunCapsuleConnectorCall {
  toolCallId: string;
  toolName: string;
  outcome: ToolOutcome | "not_finished";
  mutationBoundary: boolean;
  sequence: number;
}

export interface RunCapsuleVerification {
  verificationId: string;
  name: string;
  passed: boolean;
  summary: string;
  artifactIds: string[];
  durationMs: number;
  sequence: number;
}

export interface RunCapsuleCheckpoint {
  checkpointId: string;
  kind: string;
  label: string;
  contentSha256: string | null;
  sequence: number;
}

export interface RunCapsuleTimelineEntry {
  eventId: string;
  sequence: number;
  occurredAtMs: number;
  type: RunEventEnvelopeWire["event"]["type"];
  title: string;
  summary: string;
  actorId: string | null;
  emitter: string;
}

export interface RunCapsuleReplay {
  classification: ReplayClassification;
  boundary: "from_start" | "fresh_run_from_frozen_spec" | "inspection_only";
  safeFromStart: boolean;
  reasons: string[];
  dependencies: string[];
  externalEffects: string[];
  guarantee: string;
}

export interface RunCapsule {
  format: typeof RUN_CAPSULE_FORMAT;
  schemaVersion: typeof RUN_CAPSULE_SCHEMA_VERSION;
  run: {
    id: string;
    kind: string;
    status: RunStatus;
    createdAtMs: number;
    updatedAtMs: number;
    durationMs: number | null;
    eventCount: number;
    archivedAtMs: number | null;
  };
  prompt: {
    task: string;
    instructions: string | null;
    inputArtifactIds: string[];
  };
  target: RunRecord["spec"]["target"];
  routing: {
    rule: "frozen_target";
    description: string;
  };
  execution: {
    submittedBy: RunRecord["spec"]["submitted_by"];
    workspace: RunRecord["spec"]["workspace"];
    permissionPolicy: RunRecord["spec"]["permission_policy"];
    budgets: RunRecord["spec"]["budgets"];
  };
  tools: RunCapsuleTool[];
  approvals: RunCapsuleApproval[];
  fileChanges: RunCapsuleFileChange[];
  artifacts: RunCapsuleArtifact[];
  terminalExcerpts: RunCapsuleTerminalExcerpt[];
  connectorCalls: RunCapsuleConnectorCall[];
  verifications: RunCapsuleVerification[];
  checkpoints: RunCapsuleCheckpoint[];
  usage: UsageSnapshotWire;
  replay: RunCapsuleReplay;
  timeline: RunCapsuleTimelineEntry[];
  limitations: string[];
  sourceEvents: RunEventEnvelopeWire[];
}

export interface RunCapsuleExportEnvelope {
  format: typeof RUN_CAPSULE_FORMAT;
  schemaVersion: typeof RUN_CAPSULE_SCHEMA_VERSION;
  exportedAtMs: number;
  redaction: {
    applied: true;
    version: typeof RUN_CAPSULE_REDACTION_VERSION;
    replacements: number;
    privatePaths: "workspace_home_and_absolute_paths_aliased";
    secrets: "key_aware_and_pattern_redacted";
    binaryArtifacts: "metadata_only";
  };
  capsule: RunCapsule;
}

export interface RunCapsuleComparisonRow {
  key: string;
  label: string;
  left: string;
  right: string;
  changed: boolean;
}

export interface RunCapsuleComparison {
  leftRunId: string;
  rightRunId: string;
  rows: RunCapsuleComparisonRow[];
  sharedTools: string[];
  leftOnlyTools: string[];
  rightOnlyTools: string[];
  changedFields: number;
}

const TERMINAL_STATUSES = new Set<RunStatus>(["succeeded", "failed", "cancelled", "needs_reconciliation"]);
const FILE_MUTATION_TOOLS = new Set(["write_file", "edit_file", "apply_patch", "delete_file", "move_file", "rename_file"]);
const TERMINAL_TOOLS = new Set(["run_shell", "run_shell_command", "exec_command", "shell", "terminal"]);
const SECRET_EXPORT_FIELD = /(?:api.?key|access.?token|refresh.?token|auth.?token|authorization|password|passwd|secret|private.?key|credential.?ref|cookie)/i;
const PATH_FIELD = /^(?:path|file|file_path|filePath|target_path|targetPath|destination|output_path|outputPath)$/;

function isConnectorTool(name: string): boolean {
  return name === "mcp_call_tool" || name.startsWith("mcp__") || name.startsWith("connector__");
}

function isBrowserTool(name: string): boolean {
  return name.startsWith("browser_") || name.includes("browser") || name.includes("playwright");
}

function shortText(value: string, limit = 180): string {
  const points = [...value.replace(/\s+/g, " ").trim()];
  return points.length <= limit ? points.join("") : `${points.slice(0, limit).join("")}…`;
}

function collectPathArguments(value: unknown, key: string | null = null, output = new Set<string>()): Set<string> {
  if (typeof value === "string") {
    if (key && PATH_FIELD.test(key) && value.trim()) output.add(value.trim());
    return output;
  }
  if (Array.isArray(value)) {
    value.forEach((entry) => collectPathArguments(entry, key, output));
    return output;
  }
  if (value && typeof value === "object") {
    Object.entries(value as Record<string, unknown>).forEach(([entryKey, entry]) => {
      collectPathArguments(entry, entryKey, output);
    });
  }
  return output;
}

function timelineSummary(envelope: RunEventEnvelopeWire): { title: string; summary: string } {
  const event = envelope.event;
  switch (event.type) {
    case "queued": return { title: "Queued", summary: event.payload.queue ? `Queue: ${event.payload.queue}` : "Run entered the queue." };
    case "started": return { title: "Started", summary: `Engine: ${event.payload.engine_id}` };
    case "model_delta": return {
      title: event.payload.channel === "status" ? "Status update" : "Model output",
      summary: event.payload.channel === "status" ? shortText(event.payload.text) : `${event.payload.text.length} characters recorded`,
    };
    case "tool_proposed": return {
      title: `Tool proposed · ${event.payload.tool_name}`,
      summary: event.payload.mutation ? "Mutation-capable call; policy and approvals apply." : "Read-only call snapshot recorded.",
    };
    case "permission_requested": return { title: "Approval requested", summary: `${event.payload.tool_name}: ${shortText(event.payload.detail)}` };
    case "permission_decided": return { title: "Approval decided", summary: event.payload.decision.replace(/_/g, " ") };
    case "tool_started": return { title: "Tool started", summary: event.payload.tool_call_id };
    case "tool_finished": return { title: "Tool finished", summary: `${event.payload.outcome} in ${event.payload.duration_ms} ms` };
    case "artifact_added": return { title: "Artifact captured", summary: `${event.payload.name} · ${event.payload.media_type}` };
    case "checkpoint_linked": return { title: "Checkpoint linked", summary: event.payload.label };
    case "verification_finished": return {
      title: event.payload.passed ? "Verification passed" : "Verification failed",
      summary: `${event.payload.name}: ${shortText(event.payload.summary)}`,
    };
    case "usage_recorded": return {
      title: "Usage recorded",
      summary: `${event.payload.usage.input_tokens + event.payload.usage.output_tokens} tokens · ${event.payload.usage.tool_calls} tool calls`,
    };
    case "cancellation_requested": return { title: "Cancellation requested", summary: event.payload.reason ?? "No reason recorded." };
    case "external_mutation_prepared": return { title: "External effect prepared", summary: event.payload.summary };
    case "external_mutation_confirmed": return { title: "External effect confirmed", summary: event.payload.summary };
    case "awaiting_approval": return { title: "Awaiting approval", summary: event.payload.reason ?? event.payload.request_id };
    case "paused": return { title: "Paused", summary: event.payload.reason ?? "Paused without a recorded reason." };
    case "cancelling": return { title: "Stopping", summary: event.payload.reason ?? "Cancellation is in progress." };
    case "completed": return { title: "Completed", summary: event.payload.summary ?? "Run completed." };
    case "failed": return { title: "Failed", summary: `${event.payload.code}: ${shortText(event.payload.message)}` };
    case "cancelled": return { title: "Cancelled", summary: event.payload.reason ?? "Run cancelled." };
    case "needs_reconciliation": return { title: "Needs reconciliation", summary: event.payload.reason };
  }
}

function latestUsage(events: readonly RunEventEnvelopeWire[]): UsageSnapshotWire {
  let latest: UsageSnapshotWire = { ...EMPTY_USAGE };
  for (const envelope of events) {
    if (envelope.event.type === "usage_recorded") latest = { ...envelope.event.payload.usage };
    if (envelope.event.type === "completed") latest = { ...envelope.event.payload.usage };
  }
  return latest;
}

function replayAssessment(
  run: RunRecord,
  events: readonly RunEventEnvelopeWire[],
  tools: readonly RunCapsuleTool[],
  connectors: readonly RunCapsuleConnectorCall[],
  usage: UsageSnapshotWire,
): RunCapsuleReplay {
  const externalPrepared = events.filter((entry) => entry.event.type === "external_mutation_prepared");
  const externalConfirmed = events.filter((entry) => entry.event.type === "external_mutation_confirmed");
  const ambiguousExternal = run.status === "needs_reconciliation";
  const mutatingCalls = tools.filter((tool) => tool.mutation);
  const completedMutations = mutatingCalls.filter((tool) => tool.outcome === "succeeded");
  const completedConnectorMutations = connectors.filter((call) => call.mutationBoundary && call.outcome === "succeeded");
  const terminal = TERMINAL_STATUSES.has(run.status);
  const modelObserved = usage.model_calls > 0 || events.some((entry) => entry.event.type === "model_delta");
  const externalEffects = [
    ...externalPrepared.map((entry) => entry.event.type === "external_mutation_prepared" ? entry.event.payload.summary : ""),
    ...externalConfirmed.map((entry) => entry.event.type === "external_mutation_confirmed" ? entry.event.payload.summary : ""),
    ...completedConnectorMutations.map((call) => `${call.toolName} completed`),
  ].filter(Boolean);

  let classification: ReplayClassification;
  if (externalConfirmed.length > 0 || ambiguousExternal || completedConnectorMutations.length > 0) {
    classification = "non_repeatable";
  } else if (!modelObserved && mutatingCalls.length === 0 && connectors.length === 0) {
    classification = "deterministic";
  } else {
    classification = "best_effort";
  }

  const safeFromStart = terminal
    && classification !== "non_repeatable"
    && mutatingCalls.length === 0
    && externalPrepared.length === 0;
  const reasons: string[] = [];
  if (!terminal) reasons.push("The run is still active; replay is available only from a terminal snapshot.");
  if (modelObserved) reasons.push("Model generation is not bit-for-bit deterministic without a recorded seed and sampling contract.");
  if (mutatingCalls.length > 0) reasons.push("The run crossed a mutation-capable tool boundary; a full replay could repeat local effects.");
  if (externalPrepared.length > 0) reasons.push("An external mutation was prepared; replay is blocked even when confirmation is absent.");
  if (externalConfirmed.length > 0) reasons.push("A confirmed external effect is evidence only and is never replayed by Run Capsules.");
  if (ambiguousExternal) reasons.push("The ledger marked an external effect as needing reconciliation.");
  if (completedMutations.length === 0 && safeFromStart) reasons.push("No mutation-capable tool or external-effect boundary was recorded.");

  const dependencies = [
    `Target: ${run.spec.target.label}`,
    `Execution engine: ${events.find((entry) => entry.event.type === "started")?.event.type === "started"
      ? (events.find((entry) => entry.event.type === "started")?.event as { payload: { engine_id: string } }).payload.engine_id
      : "not recorded"}`,
    ...(run.spec.workspace?.roots.map((root) => `Workspace root: ${root.root_id} (${root.access})`) ?? []),
  ];

  return {
    classification,
    boundary: classification === "deterministic" ? "from_start" : safeFromStart ? "fresh_run_from_frozen_spec" : "inspection_only",
    safeFromStart,
    reasons,
    dependencies,
    externalEffects,
    guarantee: classification === "deterministic"
      ? "The recorded run contains no model generation, connector call, or mutation-capable tool."
      : classification === "best_effort"
        ? "A retry starts a new run from the frozen task and policy; outputs may differ and prior effects are not replayed."
        : "External effects are never replayed. Inspect evidence and reconcile the external system manually.",
  };
}

export function buildRunCapsule(run: RunRecord, sourceEvents: readonly RunEventEnvelopeWire[]): RunCapsule {
  const events = [...sourceEvents].sort((left, right) => left.sequence - right.sequence);
  const finished = new Map<string, Extract<RunEventEnvelopeWire["event"], { type: "tool_finished" }>>();
  for (const envelope of events) {
    if (envelope.event.type === "tool_finished") finished.set(envelope.event.payload.tool_call_id, envelope.event);
  }

  const tools = events.flatMap<RunCapsuleTool>((envelope) => {
    if (envelope.event.type !== "tool_proposed") return [];
    const payload = envelope.event.payload;
    const completion = finished.get(payload.tool_call_id)?.payload;
    return [{
      toolCallId: payload.tool_call_id,
      name: payload.tool_name,
      arguments: payload.arguments.value,
      argumentsRedaction: payload.arguments.redaction,
      argumentsSha256: payload.arguments_sha256,
      mutation: payload.mutation,
      outcome: completion?.outcome ?? "not_finished",
      durationMs: completion?.duration_ms ?? null,
      outputExcerpt: completion?.output_excerpt ?? null,
      proposedSequence: envelope.sequence,
    }];
  });

  const approvalsById = new Map<string, RunCapsuleApproval>();
  for (const envelope of events) {
    if (envelope.event.type === "permission_requested") {
      const payload = envelope.event.payload;
      approvalsById.set(payload.request_id, {
        requestId: payload.request_id,
        toolCallId: payload.tool_call_id,
        toolName: payload.tool_name,
        detail: payload.detail,
        operationSha256: payload.operation_sha256,
        riskLevel: payload.risk_level,
        riskReason: payload.risk_reason,
        expiresAtMs: payload.expires_at_ms,
        decision: "pending",
        requestedSequence: envelope.sequence,
        decidedSequence: null,
      });
    } else if (envelope.event.type === "permission_decided") {
      const approval = approvalsById.get(envelope.event.payload.request_id);
      if (approval) {
        approval.decision = envelope.event.payload.decision;
        approval.decidedSequence = envelope.sequence;
      }
    }
  }

  const fileChanges = tools.flatMap<RunCapsuleFileChange>((tool) => {
    if (!FILE_MUTATION_TOOLS.has(tool.name) || tool.outcome === "not_finished") return [];
    const outcome: ToolOutcome = tool.outcome;
    return [...collectPathArguments(tool.arguments)].map((path) => ({
      path,
      toolName: tool.name,
      toolCallId: tool.toolCallId,
      outcome,
      sequence: tool.proposedSequence,
    }));
  });

  const artifacts = events.flatMap<RunCapsuleArtifact>((envelope) => {
    if (envelope.event.type !== "artifact_added") return [];
    const payload = envelope.event.payload;
    return [{
      artifactId: payload.artifact_id,
      kind: payload.kind,
      name: payload.name,
      mediaType: payload.media_type,
      contentSha256: payload.content_sha256,
      sizeBytes: payload.size_bytes,
      sequence: envelope.sequence,
      browserEvidence: payload.kind === "image" || /browser|screenshot|dom|console|network/i.test(payload.name),
    }];
  });

  const terminalExcerpts = tools.filter((tool) => TERMINAL_TOOLS.has(tool.name)).map((tool) => ({
    toolCallId: tool.toolCallId,
    toolName: tool.name,
    outcome: tool.outcome,
    excerpt: tool.outputExcerpt,
    durationMs: tool.durationMs,
    sequence: tool.proposedSequence,
  }));
  const connectorCalls = tools.filter((tool) => isConnectorTool(tool.name)).map((tool) => ({
    toolCallId: tool.toolCallId,
    toolName: tool.name,
    outcome: tool.outcome,
    mutationBoundary: tool.mutation,
    sequence: tool.proposedSequence,
  }));
  const verifications = events.flatMap<RunCapsuleVerification>((envelope) => {
    if (envelope.event.type !== "verification_finished") return [];
    const payload = envelope.event.payload;
    return [{
      verificationId: payload.verification_id,
      name: payload.name,
      passed: payload.passed,
      summary: payload.summary,
      artifactIds: [...payload.artifact_ids],
      durationMs: payload.duration_ms,
      sequence: envelope.sequence,
    }];
  });
  const checkpoints = events.flatMap<RunCapsuleCheckpoint>((envelope) => {
    if (envelope.event.type !== "checkpoint_linked") return [];
    const payload = envelope.event.payload;
    return [{
      checkpointId: payload.checkpoint_id,
      kind: payload.kind,
      label: payload.label,
      contentSha256: payload.content_sha256,
      sequence: envelope.sequence,
    }];
  });
  const usage = latestUsage(events);
  const replay = replayAssessment(run, events, tools, connectorCalls, usage);
  const terminalEvent = [...events].reverse().find((entry) => ["completed", "failed", "cancelled", "needs_reconciliation"].includes(entry.event.type));
  const durationMs = terminalEvent ? Math.max(0, terminalEvent.occurred_at_ms - run.spec.created_at_ms) : null;
  const limitations = [
    "Sampling seed, temperature, and provider-side model revisions are not part of run protocol v1, so model re-runs are best-effort.",
    "Binary artifact bytes remain in the local content-addressed store; the redacted export contains verified metadata only.",
    "Shell commands can change files outside structured file-tool arguments, so the changed-file list may be incomplete.",
  ];
  if (usage.cost_micros === null) {
    limitations.push("Dollar cost is unavailable: the desktop recorder captures token and call counts without provider pricing, so cost_micros stays null.");
  }
  if (tools.some((tool) => {
    const value = tool.arguments;
    return Boolean(value && typeof value === "object" && (value as Record<string, unknown>).redacted === true);
  })) {
    limitations.push("This run predates structured evidence capture; one or more tool argument snapshots are unavailable.");
  }

  return {
    format: RUN_CAPSULE_FORMAT,
    schemaVersion: RUN_CAPSULE_SCHEMA_VERSION,
    run: {
      id: run.spec.run_id,
      kind: run.spec.kind,
      status: run.status,
      createdAtMs: run.spec.created_at_ms,
      updatedAtMs: run.updatedAtMs,
      durationMs,
      eventCount: events.length,
      archivedAtMs: run.archivedAtMs,
    },
    prompt: {
      task: run.spec.task,
      instructions: run.spec.instructions,
      inputArtifactIds: [...run.spec.input_artifact_ids],
    },
    target: structuredClone(run.spec.target),
    routing: {
      rule: "frozen_target",
      description: `The run resolved and froze ${run.spec.target.label}; replay never silently re-routes to another model.`,
    },
    execution: {
      submittedBy: structuredClone(run.spec.submitted_by),
      workspace: run.spec.workspace ? structuredClone(run.spec.workspace) : null,
      permissionPolicy: structuredClone(run.spec.permission_policy),
      budgets: structuredClone(run.spec.budgets),
    },
    tools,
    approvals: [...approvalsById.values()],
    fileChanges,
    artifacts,
    terminalExcerpts,
    connectorCalls,
    verifications,
    checkpoints,
    usage,
    replay,
    timeline: events.map((envelope) => {
      const presentation = timelineSummary(envelope);
      return {
        eventId: envelope.event_id,
        sequence: envelope.sequence,
        occurredAtMs: envelope.occurred_at_ms,
        type: envelope.event.type,
        title: presentation.title,
        summary: presentation.summary,
        actorId: envelope.actor_id,
        emitter: envelope.emitter.kind,
      };
    }),
    limitations,
    sourceEvents: structuredClone(events),
  };
}

function formatUsage(usage: UsageSnapshotWire): string {
  const tokens = usage.input_tokens + usage.output_tokens;
  const cost = usage.cost_micros === null ? "cost unknown" : `$${(usage.cost_micros / 1_000_000).toFixed(4)}`;
  return `${tokens.toLocaleString()} tokens · ${usage.model_calls} model · ${usage.tool_calls} tool · ${cost}`;
}

function verificationLabel(capsule: RunCapsule): string {
  if (capsule.verifications.length === 0) return "none";
  return `${capsule.verifications.filter((entry) => entry.passed).length}/${capsule.verifications.length} passed`;
}

export function compareRunCapsules(left: RunCapsule, right: RunCapsule): RunCapsuleComparison {
  const rawRows: Array<[string, string, string, string]> = [
    ["task", "Task", left.prompt.task, right.prompt.task],
    ["target", "Model target", left.target.label, right.target.label],
    ["status", "Status", left.run.status, right.run.status],
    ["kind", "Run kind", left.run.kind, right.run.kind],
    ["duration", "Duration", left.run.durationMs === null ? "active" : `${left.run.durationMs} ms`, right.run.durationMs === null ? "active" : `${right.run.durationMs} ms`],
    ["usage", "Usage", formatUsage(left.usage), formatUsage(right.usage)],
    ["approvals", "Approvals", String(left.approvals.length), String(right.approvals.length)],
    ["verification", "Verification", verificationLabel(left), verificationLabel(right)],
    ["replay", "Replay class", left.replay.classification, right.replay.classification],
    ["events", "Events", String(left.run.eventCount), String(right.run.eventCount)],
  ];
  const rows = rawRows.map(([key, label, leftValue, rightValue]) => ({
    key,
    label,
    left: leftValue,
    right: rightValue,
    changed: leftValue !== rightValue,
  }));
  const leftTools = new Set(left.tools.map((tool) => tool.name));
  const rightTools = new Set(right.tools.map((tool) => tool.name));
  return {
    leftRunId: left.run.id,
    rightRunId: right.run.id,
    rows,
    sharedTools: [...leftTools].filter((tool) => rightTools.has(tool)).sort(),
    leftOnlyTools: [...leftTools].filter((tool) => !rightTools.has(tool)).sort(),
    rightOnlyTools: [...rightTools].filter((tool) => !leftTools.has(tool)).sort(),
    changedFields: rows.filter((row) => row.changed).length,
  };
}

function redactAbsolutePaths(value: string): string {
  return value
    .replace(/(^|[\s([{"'=])\/(?!\/)[^\s)\]}"',;]+/g, "$1$ABSOLUTE_PATH")
    .replace(/[A-Za-z]:\\(?:[^\\\s"']+\\)+[^\\\s"']*/g, "$ABSOLUTE_PATH");
}

function exportRedact(
  value: unknown,
  roots: readonly string[],
  key: string | null,
  counter: { value: number },
): unknown {
  if (key && SECRET_EXPORT_FIELD.test(key)) {
    counter.value += 1;
    return "[REDACTED]";
  }
  if (typeof value === "string") {
    const safe = redactAbsolutePaths(redactPrivatePaths(redactSensitiveText(value), roots));
    if (safe !== value) counter.value += 1;
    return safe;
  }
  if (Array.isArray(value)) return value.map((entry) => exportRedact(entry, roots, key, counter));
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value as Record<string, unknown>).map(([entryKey, entry]) => [
      entryKey,
      exportRedact(entry, roots, entryKey, counter),
    ]));
  }
  return value;
}

export function createRedactedRunCapsuleExport(
  capsule: RunCapsule,
  exportedAtMs = Date.now(),
): RunCapsuleExportEnvelope {
  const roots = capsule.execution.workspace?.roots.map((root) => root.canonical_path) ?? [];
  const counter = { value: 0 };
  const redacted = exportRedact(capsule, roots, null, counter) as RunCapsule;
  return {
    format: RUN_CAPSULE_FORMAT,
    schemaVersion: RUN_CAPSULE_SCHEMA_VERSION,
    exportedAtMs,
    redaction: {
      applied: true,
      version: RUN_CAPSULE_REDACTION_VERSION,
      replacements: counter.value,
      privatePaths: "workspace_home_and_absolute_paths_aliased",
      secrets: "key_aware_and_pattern_redacted",
      binaryArtifacts: "metadata_only",
    },
    capsule: redacted,
  };
}

export function serializeRedactedRunCapsule(capsule: RunCapsule, exportedAtMs = Date.now()): string {
  return `${JSON.stringify(createRedactedRunCapsuleExport(capsule, exportedAtMs), null, 2)}\n`;
}

export function runCapsuleFileName(capsule: RunCapsule): string {
  const safeId = capsule.run.id.replace(/[^A-Za-z0-9_.-]+/g, "-").slice(0, 96) || "run";
  return `little-monkey-${safeId}.lmcapsule.json`;
}

export function capsuleHasBrowserEvidence(capsule: RunCapsule): boolean {
  return capsule.artifacts.some((artifact) => artifact.browserEvidence)
    || capsule.tools.some((tool) => isBrowserTool(tool.name));
}
