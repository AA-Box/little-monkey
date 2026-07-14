import { isTauri } from "@tauri-apps/api/core";

import type { ModelTargetSnapshot, CapabilityAssessment } from "./modelTargets";
import {
  EMPTY_USAGE,
  RUN_PROTOCOL_SCHEMA_VERSION,
  appendRunEvent,
  runProtocolVersion,
  submitRun,
  type ModelCapabilitiesSnapshotWire,
  type ModelTargetSnapshotWire,
  type PermissionPolicySnapshotWire,
  type RunBudgetsWire,
  type RunEventWire,
  type RunKind,
  type RunSpecWire,
  type ToolOutcome,
  type UsageSnapshotWire,
  type WorkspaceContextWire,
} from "./runProtocol";
import type { PermissionMode } from "../store/permissionStore";
import type { WorkspaceRootInfo } from "../store/workspaceStore";

const MUTATING_TOOLS = new Set([
  "write_file",
  "edit_file",
  "run_shell",
  "remember",
  "git_commit",
  "mcp_call_tool",
]);
const EVENT_TEXT_CHUNK_BYTES = 24_000;
export function redactSensitiveText(value: string): string {
  return value
    .replace(/-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----/g, "[REDACTED PRIVATE KEY]")
    .replace(/\bBearer\s+[A-Za-z0-9._~+/=-]{12,}/gi, "Bearer [REDACTED]")
    .replace(/\b(?:sk|pk|ghp|gho|github_pat|xox[baprs])-[-A-Za-z0-9_]{10,}\b/g, "[REDACTED TOKEN]")
    .replace(
      /\b(api[_-]?key|access[_-]?token|refresh[_-]?token|auth[_-]?token|password|passwd|secret)\b\s*[:=]\s*["']?[^\s"',;}]+["']?/gi,
      "$1=[REDACTED]",
    )
    .replace(/(https?:\/\/)[^\s/@:]+:[^\s/@]+@/gi, "$1[REDACTED]@/");
}

function capabilityState(value: CapabilityAssessment["state"]): "supported" | "unsupported" | "unknown" {
  if (value === "yes") return "supported";
  if (value === "no") return "unsupported";
  return "unknown";
}

function capability(value: CapabilityAssessment): { state: "supported" | "unsupported" | "unknown"; evidence: string } {
  return { state: capabilityState(value.state), evidence: value.evidence };
}

const UNKNOWN_CAPABILITY = Object.freeze({
  state: "unknown" as const,
  evidence: "This target inventory does not report this capability.",
});

export function modelCapabilitiesToRunWire(target: ModelTargetSnapshot): ModelCapabilitiesSnapshotWire {
  return {
    tool_calling: capability(target.capabilities.toolCalling),
    vision: capability(target.capabilities.vision),
    embeddings: { ...UNKNOWN_CAPABILITY },
    structured_output: { ...UNKNOWN_CAPABILITY },
    image_generation: { ...UNKNOWN_CAPABILITY },
    audio: { ...UNKNOWN_CAPABILITY },
    runtime_lifecycle: {
      state: target.kind === "provider" ? "unsupported" : "supported",
      evidence:
        target.kind === "provider"
          ? "Little Monkey does not manage provider runtime lifecycle."
          : "The selected local runtime exposes lifecycle controls.",
    },
    fim: { ...UNKNOWN_CAPABILITY },
    code_completion: { ...UNKNOWN_CAPABILITY },
    inline_edit: { ...UNKNOWN_CAPABILITY },
    fim_metadata: null,
  };
}

function stableProtocolId(prefix: string, value: string): string {
  let hash = 0x811c9dc5;
  for (const byte of new TextEncoder().encode(value)) {
    hash ^= byte;
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  const readable = value.replace(/[^A-Za-z0-9_.:-]+/g, "_").replace(/^[^A-Za-z0-9]+|[^A-Za-z0-9]+$/g, "");
  const suffix = hash.toString(16).padStart(8, "0");
  return `${prefix}-${(readable || "target").slice(0, 96)}-${suffix}`;
}

export function protocolToolCallId(value: string): string {
  return stableProtocolId("tool", value);
}

/** Converts the UI inventory record into the stricter cross-client wire
 * snapshot. Provider credentials remain an opaque keychain reference. */
export function modelTargetToRunWire(target: ModelTargetSnapshot): ModelTargetSnapshotWire {
  const common = {
    target_id: stableProtocolId("target", target.key),
    label: `${target.label} · ${target.displayName}`,
    capabilities: modelCapabilitiesToRunWire(target),
  };
  if (target.kind === "local") {
    return {
      kind: "managed_llama",
      ...common,
      model_id: target.modelId,
      model_path: target.modelPath,
      estimated_memory_bytes: target.estimatedMemoryBytes ?? null,
    };
  }
  if (target.kind === "ollama") {
    return {
      kind: "ollama",
      ...common,
      base_url: target.baseUrl,
      model: target.model,
      is_cloud: target.isCloud ?? false,
      estimated_memory_bytes: target.estimatedMemoryBytes ?? null,
    };
  }
  return {
    kind: "provider",
    ...common,
    provider_id: target.providerId,
    endpoint: target.endpoint,
    model: target.model,
    credential_ref_id: target.credentialRefId,
  };
}

export function workspaceToRunWire(
  roots: readonly WorkspaceRootInfo[],
  access: "read_only" | "read_write" = "read_write",
): WorkspaceContextWire | null {
  const primary = roots.find((root) => root.is_primary);
  if (!primary) return null;
  return {
    workspace_id: stableProtocolId("workspace", primary.path),
    primary_root_id: primary.id,
    roots: roots.map((root) => ({
      root_id: root.id,
      canonical_path: root.path,
      access,
      allow_symlinks_within_root: false,
    })),
    repository_policy: {
      root_id: primary.id,
      owned_worktree_required: false,
      allowed_remote_names: [],
      allowed_branch_prefixes: [],
      allow_commit: access === "read_write",
      allow_push: false,
      allow_create_pull_request: false,
      allow_review_comment: false,
      allow_merge: false,
      allow_force_push: false,
    },
  };
}

export function permissionPolicyForRun(
  mode: PermissionMode,
  options: { unattended?: boolean; allowNetwork?: boolean; allowExternalMutations?: boolean } = {},
): PermissionPolicySnapshotWire {
  const allowEdits = mode === "acceptEdits" || mode === "auto";
  const tool_rules = allowEdits
    ? ["write_file", "edit_file", "remember"].map((tool) => ({ tool, decision: "allow" as const }))
    : [];
  return {
    mode,
    unattended: options.unattended ?? false,
    approval_timeout_ms: 5 * 60 * 1_000,
    default_tool_decision: mode === "plan" ? "deny" : mode === "bypass" ? "allow" : "prompt",
    tool_rules,
    allow_network: options.allowNetwork ?? false,
    allow_external_mutations: options.allowExternalMutations ?? false,
  };
}

export function defaultRunBudgets(noTools = false): RunBudgetsWire {
  return {
    wall_time_ms: 30 * 60 * 1_000,
    max_iterations: 32,
    max_model_calls: 64,
    max_tool_calls: noTools ? 0 : 128,
    max_input_tokens: 1_000_000,
    max_output_tokens: 250_000,
    max_cost_micros: null,
    max_artifact_bytes: 256 * 1024 * 1024,
    max_event_count: 20_000,
  };
}

export interface BeginDurableRunOptions {
  runId: string;
  idempotencyKey?: string;
  kind: RunKind;
  task: string;
  instructions?: string | null;
  target: ModelTargetSnapshot;
  roots: readonly WorkspaceRootInfo[];
  permissionMode: PermissionMode;
  allowNetwork?: boolean;
  allowExternalMutations?: boolean;
  budgets?: RunBudgetsWire;
  actorId?: string | null;
  workspaceAccess?: "read_only" | "read_write";
}

async function sha256(value: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function utf8Chunks(value: string, size = EVENT_TEXT_CHUNK_BYTES): string[] {
  if (!value) return [];
  if (!Number.isSafeInteger(size) || size < 4) {
    throw new Error("Run-event chunk size must be an integer of at least four UTF-8 bytes.");
  }
  const encoder = new TextEncoder();
  const result: string[] = [];
  let current = "";
  let currentBytes = 0;
  for (const codePoint of value) {
    const codePointBytes = encoder.encode(codePoint).byteLength;
    if (current && currentBytes + codePointBytes > size) {
      result.push(current);
      current = "";
      currentBytes = 0;
    }
    current += codePoint;
    currentBytes += codePointBytes;
  }
  if (current) result.push(current);
  return result;
}

function resultOutcome(result: string, cancelled: boolean): ToolOutcome {
  if (cancelled) return "cancelled";
  try {
    const parsed = JSON.parse(result) as { error?: unknown };
    if (parsed && typeof parsed === "object" && parsed.error) {
      return String(parsed.error).toLowerCase().includes("permission") ? "denied" : "failed";
    }
  } catch {
    // Successful plain-text tool results are expected.
  }
  return "succeeded";
}

/** Serial event writer for one active desktop run. Calls are queued so
 * concurrent tool completions cannot race the ledger sequence. */
export class DurableRunRecorder {
  readonly runId: string;
  readonly actorId: string | null;
  private tail: Promise<void> = Promise.resolve();
  private firstError: unknown = null;
  private terminal = false;
  private usage: UsageSnapshotWire = { ...EMPTY_USAGE };

  constructor(runId: string, actorId: string | null = null) {
    this.runId = runId;
    this.actorId = actorId;
  }

  private enqueue(event: RunEventWire, actorId: string | null = this.actorId): Promise<void> {
    this.tail = this.tail.then(async () => {
      if (this.firstError) return;
      try {
        await appendRunEvent(this.runId, event, actorId);
      } catch (error) {
        this.firstError = error;
      }
    });
    return this.tail;
  }

  async flush(): Promise<void> {
    await this.tail;
    if (this.firstError) throw this.firstError;
  }

  recordModelOutput(messageId: string, text: string, actorId: string | null = this.actorId): void {
    for (const part of utf8Chunks(redactSensitiveText(text))) {
      void this.enqueue({ type: "model_delta", payload: { message_id: messageId, channel: "assistant", text: part } }, actorId);
    }
  }

  recordStatus(messageId: string, text: string): void {
    for (const part of utf8Chunks(redactSensitiveText(text))) {
      void this.enqueue({ type: "model_delta", payload: { message_id: messageId, channel: "status", text: part } });
    }
  }

  async recordToolProposed(toolCallId: string, toolName: string, rawArguments: string, actorId: string | null = this.actorId): Promise<void> {
    const protocolId = protocolToolCallId(toolCallId);
    const argumentsSha256 = await sha256(rawArguments || "{}");
    await this.enqueue({
      type: "tool_proposed",
      payload: {
        tool_call_id: protocolId,
        tool_name: toolName,
        arguments: { value: { redacted: true }, redaction: "applied" },
        arguments_sha256: argumentsSha256,
        mutation: MUTATING_TOOLS.has(toolName) || toolName.startsWith("mcp__"),
      },
    }, actorId);
  }

  recordToolStarted(toolCallId: string, actorId: string | null = this.actorId): void {
    void this.enqueue({ type: "tool_started", payload: { tool_call_id: protocolToolCallId(toolCallId) } }, actorId);
  }

  async recordToolFinished(
    toolCallId: string,
    result: string,
    durationMs: number,
    cancelled = false,
    actorId: string | null = this.actorId,
  ): Promise<void> {
    const outcome = resultOutcome(result, cancelled);
    this.usage = { ...this.usage, tool_calls: this.usage.tool_calls + 1 };
    await this.enqueue({
      type: "tool_finished",
      payload: {
        tool_call_id: protocolToolCallId(toolCallId),
        outcome,
        output_excerpt: null,
        output_sha256: await sha256(result),
        duration_ms: Math.max(0, Math.trunc(durationMs)),
      },
    }, actorId);
  }

  recordUsage(inputTokens: number, outputTokens: number): void {
    this.usage = {
      ...this.usage,
      input_tokens: this.usage.input_tokens + Math.max(0, Math.trunc(inputTokens)),
      output_tokens: this.usage.output_tokens + Math.max(0, Math.trunc(outputTokens)),
      model_calls: this.usage.model_calls + 1,
    };
    void this.enqueue({ type: "usage_recorded", payload: { usage: { ...this.usage } } });
  }

  recordCheckpoint(checkpointId: string, label: string): void {
    void this.enqueue({
      type: "checkpoint_linked",
      payload: { checkpoint_id: checkpointId, kind: "workspace", label: label || "Workspace checkpoint", content_sha256: null },
    });
  }

  recordVerification(name: string, passed: boolean, summary: string, durationMs: number): void {
    void this.enqueue({
      type: "verification_finished",
      payload: {
        verification_id: stableProtocolId("verify", `${this.runId}:${name}:${Date.now()}`),
        name,
        passed,
        summary: redactSensitiveText(summary).slice(0, EVENT_TEXT_CHUNK_BYTES),
        artifact_ids: [],
        duration_ms: Math.max(0, Math.trunc(durationMs)),
      },
    });
  }

  async complete(summary: string | null = null): Promise<void> {
    if (this.terminal) return this.flush();
    this.terminal = true;
    await this.enqueue({
      type: "completed",
      payload: { summary: summary === null ? null : redactSensitiveText(summary), result_artifact_ids: [], usage: { ...this.usage } },
    });
    await this.flush();
  }

  async fail(error: unknown, retryable = false): Promise<void> {
    if (this.terminal) return this.flush();
    this.terminal = true;
    const message = redactSensitiveText(error instanceof Error ? error.message : String(error));
    await this.enqueue({ type: "failed", payload: { code: "desktop_turn_failed", message, retryable } });
    await this.flush();
  }

  async cancel(reason: string | null = null): Promise<void> {
    if (this.terminal) return this.flush();
    this.terminal = true;
    await this.enqueue({ type: "cancelling", payload: { reason } });
    await this.enqueue({ type: "cancelled", payload: { reason } });
    await this.flush();
  }
}

/** Starts a real ledger-backed desktop run inside Tauri. Browser-only UI
 * development keeps working and simply returns `null`. */
export async function beginDurableRun(options: BeginDurableRunOptions): Promise<DurableRunRecorder | null> {
  if (!isTauri()) return null;
  // Older desktop hosts can still render a newer frontend during development
  // or a staged update. They keep the legacy turn path instead of receiving
  // events whose contract they cannot validate.
  if (await runProtocolVersion().catch(() => 0) !== RUN_PROTOCOL_SCHEMA_VERSION) return null;
  const spec: RunSpecWire = {
    schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
    run_id: options.runId,
    idempotency_key: options.idempotencyKey ?? `desktop/${options.runId}`,
    created_at_ms: Date.now(),
    kind: options.kind,
    submitted_by: {
      client_id: "little-monkey-desktop",
      instance_id: "webview",
      kind: "desktop",
      version: "0.1.0",
    },
    task: redactSensitiveText(options.task.trim() || "Attachment turn"),
    instructions: options.instructions === null || options.instructions === undefined
      ? null
      : redactSensitiveText(options.instructions),
    input_artifact_ids: [],
    target: modelTargetToRunWire(options.target),
    workspace: workspaceToRunWire(options.roots, options.workspaceAccess),
    permission_policy: permissionPolicyForRun(options.permissionMode, {
      allowNetwork: options.allowNetwork,
      allowExternalMutations: options.allowExternalMutations,
    }),
    budgets: options.budgets ?? defaultRunBudgets(false),
  };
  await submitRun(spec);
  const recorder = new DurableRunRecorder(options.runId, options.actorId ?? null);
  await appendRunEvent(options.runId, { type: "queued", payload: { queue: "desktop-interactive" } }, recorder.actorId);
  await appendRunEvent(options.runId, { type: "started", payload: { engine_id: "desktop-turn-engine-v1" } }, recorder.actorId);
  return recorder;
}
