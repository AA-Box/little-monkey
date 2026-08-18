import type { ChatContentPart, ChatMessage } from "./llamaClient";
import { isTauri } from "@tauri-apps/api/core";
import type { ResolvedTarget } from "./turnEngine";
import {
  backpressureGate,
  backpressureMessage,
  backpressureOf,
  daemonCancel,
  daemonDesktopTurnSubmit,
  daemonStatus,
  type DaemonStatus,
  type DaemonTurnSubmitResponse,
} from "./daemonClient";
import {
  getRun,
  loadRunEvents,
  type ModelTargetSnapshotWire,
  type PermissionPolicySnapshotWire,
  type RunEventEnvelopeWire,
  type RunRecord,
  type WorkspaceContextWire,
} from "./runProtocol";
import {
  modelTargetToRunWire,
  permissionPolicyForRun,
  protocolRootId,
  workspaceToRunWire,
} from "./durableRun";
import { ingressTurnResume, ingressTurnShow, isRefusedResume } from "./ingressClient";
import type { ModelTargetSnapshot } from "./modelTargets";
import type { PermissionMode } from "../store/permissionStore";
import type { WorkspaceRootInfo } from "../store/workspaceStore";
import type { McpServerInfo } from "../store/mcpStore";

export const DAEMON_DESKTOP_TURN_SCHEMA_VERSION = 3 as const;
const ACTIVE_TURNS_KEY = "little-monkey-daemon-desktop-turns-v1";
const POLL_INTERVAL_MS = 150;

export interface FrozenAttachmentInput {
  path: string;
  kind: "file" | "directory" | "image";
  mediaType: string;
  content: string;
}

interface FrozenAttachmentWire {
  path: string;
  kind: string;
  media_type: string;
  content: string;
  content_sha256: string;
  size_bytes: number;
}

interface FrozenGenerationSettingsWire {
  temperature: number | null;
  top_p: number | null;
  seed: number | null;
  stop: string[];
  num_ctx: number | null;
  num_predict: number | null;
  format: unknown | null;
  think: boolean | "low" | "medium" | "high" | null;
  hide_thinking: boolean;
  keep_alive: string | null;
  effort: "low" | "medium" | "high" | "xhigh" | "max" | null;
}

interface FrozenToolProfileWire {
  memory_enabled: boolean;
  web_tools_enabled: boolean;
  verify_enabled: boolean;
  verify_max_rounds: number;
  subagents_enabled: boolean;
}

interface FrozenMcpServerWire {
  id: string;
  config_sha256: string;
  tool_allowlist: string[] | null;
}

interface DesktopTurnRecipe {
  version: 1;
  name: string;
  description: string;
  target: {
    provider: string | null;
    model: string | null;
    ollama: string | null;
    local_url: string | null;
  };
  workspace: string | null;
  permission_mode: PermissionMode;
  system: string;
  prompt: string;
  params: Record<string, never>;
  max_iterations: number;
  timeout_seconds: number;
  output: { json: true };
  desktop_turn: {
    schema_version: typeof DAEMON_DESKTOP_TURN_SCHEMA_VERSION;
    session_id: string;
    turn_id: string;
    submitted_at_ms: number;
    execution_base_url: string | null;
    history: unknown[];
    target: ModelTargetSnapshotWire;
    workspace: WorkspaceContextWire | null;
    execution_roots: Array<{
      root_id: string;
      canonical_path: string;
      label: string;
      is_primary: boolean;
    }>;
    permission_policy: PermissionPolicySnapshotWire;
    generation: FrozenGenerationSettingsWire;
    tool_profile: FrozenToolProfileWire;
    mcp_servers: FrozenMcpServerWire[];
    attached_stack_ids: string[];
    attached_stack_names: string[];
    attachments: FrozenAttachmentWire[];
    /** Whether this turn promised the workspace would be different afterwards.
     * Frozen here so the runtime checks the promise the turn was accepted with
     * — not one re-derived from the prompt at execution time — and so the
     * durable policy, not this process, owns what happens when it is unmet. */
    workspace_mutation_required: boolean;
  };
}

export interface BuildDaemonDesktopRecipeOptions {
  sessionId: string;
  turnId: string;
  submittedAtMs?: number;
  userText: string;
  systemPrompt: string;
  history: ChatMessage[];
  resolvedTarget: ResolvedTarget;
  targetSnapshot: ModelTargetSnapshot;
  roots: readonly WorkspaceRootInfo[];
  permissionMode: PermissionMode;
  allowNetwork: boolean;
  memoryEnabled: boolean;
  verifyEnabled: boolean;
  verifyMaxRounds: number;
  subagentsEnabled: boolean;
  effort: FrozenGenerationSettingsWire["effort"];
  generation?: Partial<Omit<FrozenGenerationSettingsWire, "effort">>;
  mcpServers: readonly McpServerInfo[];
  attachedStackIds: readonly string[];
  attachedStackNames: readonly string[];
  attachments: FrozenAttachmentInput[];
  workspaceMutationRequired: boolean;
}

export interface ActiveDaemonDesktopTurn {
  sessionId: string;
  turnId: string;
  runId: string;
  assistantIndex: number;
  lastSequence: number;
  output: string;
  /** Which of the operator's own surfaces submitted the turn — the origin half
   * of its durable identity, needed to ask the backend about it. Absent on a
   * link stored by an older build, which was always the composer. */
  source?: DesktopTurnSource;
}

export interface DaemonTurnProjection {
  output: string;
  status: string;
  terminal: boolean;
  terminalStatus: RunRecord["status"] | null;
  error: string | null;
  summary: string | null;
  lastSequence: number;
}

export interface WatchDaemonTurnCallbacks {
  onProjection: (projection: DaemonTurnProjection) => void;
  onLinkChanged?: (link: ActiveDaemonDesktopTurn) => void;
}

function textParts(content: ChatMessage["content"]): { text: string; images: string[] } {
  if (typeof content === "string") return { text: content, images: [] };
  const text = content
    .filter((part): part is Extract<ChatContentPart, { type: "text" }> => part.type === "text")
    .map((part) => part.text)
    .join("\n");
  const images = content
    .filter((part): part is Extract<ChatContentPart, { type: "image_url" }> => part.type === "image_url")
    .map((part) => part.image_url.url);
  return { text, images };
}

function base64FromDataUrl(value: string): string {
  const comma = value.indexOf(",");
  return comma >= 0 ? value.slice(comma + 1) : value;
}

/** Converts the persisted OpenAI-style transcript to the exact message
 * shape accepted by the frozen target. In particular, native Ollama expects
 * top-level `images` and object-valued tool arguments. */
export function historyForDaemonTarget(history: readonly ChatMessage[], target: ResolvedTarget): unknown[] {
  if (target.kind !== "ollama") return history.map((message) => structuredClone(message));
  const toolNames = new Map<string, string>();
  return history.map((message) => {
    const { text, images } = textParts(message.content);
    if (message.role === "assistant" && message.tool_calls) {
      const tool_calls = message.tool_calls.map((call) => {
        toolNames.set(call.id, call.function.name);
        let args: unknown = {};
        try {
          args = JSON.parse(call.function.arguments || "{}");
        } catch {
          args = {};
        }
        return { function: { name: call.function.name, arguments: args } };
      });
      return { role: "assistant", content: text, tool_calls };
    }
    if (message.role === "tool") {
      return {
        role: "tool",
        tool_name: message.tool_call_id ? toolNames.get(message.tool_call_id) ?? "tool" : "tool",
        content: text,
      };
    }
    if (message.role === "user" && images.length > 0) {
      return { role: "user", content: text, images: images.map(base64FromDataUrl) };
    }
    return { role: message.role, content: text };
  });
}

function recipeTarget(target: ResolvedTarget, snapshot: ModelTargetSnapshot): DesktopTurnRecipe["target"] {
  if (target.kind === "provider") {
    return { provider: target.providerId, model: target.model, ollama: null, local_url: null };
  }
  if (target.kind === "ollama") {
    return { provider: null, model: null, ollama: target.model, local_url: null };
  }
  if (!target.baseUrl) throw new Error("The managed runtime did not expose an execution origin.");
  return {
    provider: null,
    model: snapshot.kind === "local" ? snapshot.modelId : (target.modelLabel ?? "local"),
    ollama: null,
    local_url: target.baseUrl,
  };
}

async function sha256(value: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function stableJson(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
  if (Array.isArray(value)) return `[${value.map(stableJson).join(",")}]`;
  const record = value as Record<string, unknown>;
  return `{${Object.keys(record).sort().map((key) => `${JSON.stringify(key)}:${stableJson(record[key])}`).join(",")}}`;
}

function normalizedAllowlist(value: readonly string[] | null): string[] | null {
  return value ? [...new Set(value)].sort() : null;
}

async function frozenMcpServers(servers: readonly McpServerInfo[]): Promise<FrozenMcpServerWire[]> {
  return Promise.all(servers
    .filter((server) => server.enabled && server.status === "connected")
    .map(async (server) => {
      const toolAllowlist = normalizedAllowlist(server.toolAllowlist);
      const normalizedEntry = {
        id: server.id,
        label: server.label,
        transport: server.transport,
        enabled: server.enabled,
        tool_allowlist: toolAllowlist,
        timeout_secs: server.timeoutSecs,
      };
      return {
        id: server.id,
        config_sha256: await sha256(stableJson(normalizedEntry)),
        tool_allowlist: toolAllowlist,
      };
    }));
}

function safeRecipeName(turnId: string): string {
  const suffix = turnId.toLowerCase().replace(/[^a-z0-9-]+/g, "-").replace(/^-+|-+$/g, "");
  return `desktop-${suffix || "turn"}`.slice(0, 120).replace(/-+$/g, "") || "desktop-turn";
}

export async function buildDaemonDesktopRecipe(
  options: BuildDaemonDesktopRecipeOptions,
): Promise<DesktopTurnRecipe> {
  const workspace = workspaceToRunWire(options.roots);
  const primary = options.roots.find((root) => root.is_primary);
  const attachments: FrozenAttachmentWire[] = await Promise.all(options.attachments.map(async (attachment) => ({
    path: attachment.path,
    kind: attachment.kind,
    media_type: attachment.mediaType,
    content: attachment.content,
    content_sha256: await sha256(attachment.content),
    size_bytes: new TextEncoder().encode(attachment.content).byteLength,
  })));
  const permission = permissionPolicyForRun(options.permissionMode, {
    unattended: true,
    allowNetwork: options.allowNetwork,
    allowExternalMutations: false,
  });
  const generation: FrozenGenerationSettingsWire = {
    temperature: options.generation?.temperature ?? null,
    top_p: options.generation?.top_p ?? null,
    seed: options.generation?.seed ?? null,
    stop: options.generation?.stop ? [...options.generation.stop] : [],
    num_ctx: options.generation?.num_ctx ?? null,
    num_predict: options.generation?.num_predict ?? null,
    format: options.generation?.format ?? null,
    think: options.generation?.think ?? null,
    hide_thinking: options.generation?.hide_thinking ?? false,
    keep_alive: options.generation?.keep_alive ?? null,
    effort: options.effort,
  };
  return {
    version: 1,
    name: safeRecipeName(options.turnId),
    description: `Immutable desktop turn for session ${options.sessionId}`,
    target: recipeTarget(options.resolvedTarget, options.targetSnapshot),
    workspace: primary?.path ?? null,
    permission_mode: options.permissionMode,
    system: options.systemPrompt,
    prompt: options.userText.trim() || "Attachment turn",
    params: {},
    max_iterations: 25,
    timeout_seconds: 30 * 60,
    output: { json: true },
    desktop_turn: {
      schema_version: DAEMON_DESKTOP_TURN_SCHEMA_VERSION,
      session_id: options.sessionId,
      turn_id: options.turnId,
      submitted_at_ms: options.submittedAtMs ?? Date.now(),
      execution_base_url: options.resolvedTarget.kind === "provider" ? null : options.resolvedTarget.baseUrl,
      history: historyForDaemonTarget(options.history, options.resolvedTarget),
      target: modelTargetToRunWire(options.targetSnapshot),
      workspace,
      execution_roots: options.roots.map((root) => ({
        root_id: protocolRootId(root),
        canonical_path: root.path,
        label: root.label,
        is_primary: root.is_primary,
      })),
      permission_policy: permission,
      generation,
      tool_profile: {
        memory_enabled: options.memoryEnabled,
        web_tools_enabled: options.allowNetwork,
        verify_enabled: options.verifyEnabled,
        verify_max_rounds: options.verifyMaxRounds,
        subagents_enabled: options.subagentsEnabled,
      },
      mcp_servers: await frozenMcpServers(options.mcpServers),
      attached_stack_ids: [...options.attachedStackIds],
      attached_stack_names: [...options.attachedStackNames],
      attachments,
      workspace_mutation_required: options.workspaceMutationRequired,
    },
  };
}

/**
 * Where a conversational turn is allowed to execute.
 *
 * `browser` is a profile with no Tauri bridge at all — a dev server in a plain
 * browser tab, where no durable execution authority exists on the machine and
 * the in-process loop is the only thing there is. `daemon` is the resident
 * runner. The packaged desktop app is always the second one: there is no third
 * value for "desktop without a runner", because that state is a refusal rather
 * than a different place to run.
 */
export type ConversationRoute = "browser" | "daemon";

/**
 * The execution service is missing or unhealthy — a repairable fault in the
 * app's own runtime, not something the operator chose.
 *
 * Typed rather than a plain `Error` so a surface can offer to fix it in place.
 * That distinction is the whole point: the service is infrastructure every chat
 * turn runs on, installed and started by the app itself, so the answer to "it
 * isn't there" is a Repair button — never a sentence sending someone who wanted
 * to send a message off to install a background-agents feature.
 *
 * Deliberately NOT used for the kill switch or backpressure below: those are
 * states an operator or the scheduler chose, and repairing the service would be
 * the wrong response to both.
 */
export class ExecutionServiceUnavailable extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ExecutionServiceUnavailable";
  }
}

/** Whether a caught value is a repairable execution-service fault. */
export function isExecutionServiceUnavailable(error: unknown): error is ExecutionServiceUnavailable {
  return error instanceof ExecutionServiceUnavailable;
}

/**
 * The route for a machine that HAS the bridge — so, only ever `daemon`.
 *
 * Every unhealthy state throws instead of returning, and the return type says so:
 * a caller cannot accidentally treat a stopped, stale, kill-switched or missing
 * runner as permission to execute somewhere else. Execution authority is not a
 * thing this app silently changes.
 */
export function daemonRouteFromStatus(status: DaemonStatus): "daemon" {
  if (!status.installed) {
    throw new ExecutionServiceUnavailable("Little Monkey's execution service isn't installed yet, so this turn has nowhere to run.");
  }
  if (!status.serviceRunning || !status.heartbeatFresh) {
    throw new ExecutionServiceUnavailable("Little Monkey's execution service is not healthy, so this turn has nowhere to run.");
  }
  if (status.killSwitch) throw new Error("The M6A global kill switch is engaged.");
  // K8 backpressure, as an INTERACTIVE producer: a user is waiting on this turn.
  //
  // `closed` refuses before anything is created — the daemon's own `enqueue`
  // would refuse anyway, so attempting it just trades a sentence the user can
  // act on ("cancel a run", "release the kill switch") for a generic error.
  // `slow` proceeds: it is a request to defer, and there is nothing to defer
  // *to* here — the person is sitting in front of the turn. Deferring an
  // interactive turn is a refusal they did not ask for.
  //
  // An absent `backpressure` field (older daemon) reads as accepting, so this
  // adds no new way for the app to stop working.
  const gate = backpressureGate(backpressureOf(status), "interactive");
  if (!gate.proceed) {
    throw new Error(backpressureMessage(
      gate.signal,
      "The resident runner is not accepting work right now.",
      (retryAfterMs) => `Try again in about ${Math.round(retryAfterMs / 1_000)}s.`,
    ));
  }
  return "daemon";
}

/** The only place `browser` can come from: no Tauri bridge, so no resident
 * runner can exist. Once the bridge is there, the turn belongs to the durable
 * backend or it does not run at all. */
export async function daemonDesktopRoute(): Promise<ConversationRoute> {
  if (!isTauri()) return "browser";
  return daemonRouteFromStatus(await daemonStatus());
}

/** Which of the operator's own surfaces a turn was spoken or typed on. */
export type DesktopTurnSource = "desktop" | "voice";

/** How many times one send may reach the bridge. */
const SUBMIT_ATTEMPTS = 3;

/**
 * Hands one turn to the resident runner, retrying a transport failure under
 * the SAME turn id.
 *
 * The id is what makes the retry safe. A bridge call can time out after the
 * daemon has already accepted the turn, so a second attempt has to be able to
 * land on the run the first one created — which it does, because the turn id
 * is the ingress dedupe identity and the daemon answers a repeat with the job
 * it already has. Minting a fresh id per attempt is the bug this exists to
 * prevent: it would turn one send into two runs.
 */
export async function submitDaemonDesktopTurn(
  turnId: string,
  recipe: DesktopTurnRecipe,
  source: DesktopTurnSource = "desktop",
): Promise<DaemonTurnSubmitResponse> {
  let lastError: unknown;
  for (let attempt = 0; attempt < SUBMIT_ATTEMPTS; attempt += 1) {
    try {
      return await daemonDesktopTurnSubmit({ turnId, recipe, source });
    } catch (error) {
      lastError = error;
      if (attempt + 1 < SUBMIT_ATTEMPTS) await wait(POLL_INTERVAL_MS * (attempt + 1));
    }
  }
  throw lastError;
}

/** The durable continuation one Resume produced, once the backend has it. */
export interface AcceptedResume {
  /** The continuation's own accepted-turn id. */
  ingressId: string;
  /** The accepted turn it continues. */
  parentIngressId: string;
  jobId: string;
  runId: string;
}

/** What a Resume submission settled on, from the caller's point of view.
 *
 * `pending` is the answer that matters: nothing here can say whether the backend
 * took the request, so nothing downstream may act as though it did — the frozen
 * image stays, the suspended process stays, and the same request id is sent
 * again later. */
export type ResumeSubmissionOutcome =
  | { state: "accepted"; accepted: AcceptedResume }
  | { state: "refused"; reason: string }
  | { state: "pending"; error: unknown };

/**
 * Asks the durable backend to continue a frozen turn, retrying the transport
 * under the SAME request id.
 *
 * The retry is safe for the reason {@link submitDaemonDesktopTurn}'s is: the id
 * is the continuation's identity, so a bridge call that timed out *after* the
 * backend accepted is answered on the next attempt with the continuation that
 * already exists rather than a second one. Minting an id per attempt would turn
 * one Resume into several runs of the same work, which is the one outcome
 * nothing downstream can undo.
 *
 * Every failure that survives the retries is reported as `pending`, never as a
 * failure: this side cannot distinguish "never arrived" from "arrived, answer
 * lost", and only the backend's own refusal — which comes back as a value —
 * settles anything. A caller therefore keeps everything it would need to ask
 * again.
 */
export async function submitDurableResume(
  source: DesktopTurnSource,
  sessionKey: string,
  parentTurnId: string,
  requestId: string,
): Promise<ResumeSubmissionOutcome> {
  let lastError: unknown;
  for (let attempt = 0; attempt < SUBMIT_ATTEMPTS; attempt += 1) {
    try {
      const submission = await ingressTurnResume(source, sessionKey, parentTurnId, requestId);
      return isRefusedResume(submission)
        ? { state: "refused", reason: submission.refused }
        : {
          state: "accepted",
          accepted: {
            ingressId: submission.ingress_id,
            parentIngressId: submission.parent_ingress_id,
            jobId: submission.job_id,
            runId: submission.run_id,
          },
        };
    } catch (error) {
      lastError = error;
      if (attempt + 1 < SUBMIT_ATTEMPTS) await wait(POLL_INTERVAL_MS * (attempt + 1));
    }
  }
  return { state: "pending", error: lastError };
}

function emptyProjection(link: ActiveDaemonDesktopTurn): DaemonTurnProjection {
  return {
    output: link.output,
    status: "Queued in the resident runner…",
    terminal: false,
    terminalStatus: null,
    error: null,
    summary: null,
    lastSequence: link.lastSequence,
  };
}

export function projectDaemonTurnEvents(
  previous: DaemonTurnProjection,
  events: readonly RunEventEnvelopeWire[],
  run: RunRecord | null,
): DaemonTurnProjection {
  let next = { ...previous };
  for (const envelope of events) {
    if (envelope.sequence <= next.lastSequence) continue;
    next.lastSequence = envelope.sequence;
    const event = envelope.event;
    switch (event.type) {
      case "queued": next.status = "Queued in the resident runner…"; break;
      case "started": next.status = "Resident agent is working…"; break;
      case "model_delta":
        if (event.payload.channel === "assistant") next.output += event.payload.text;
        else next.status = event.payload.text;
        break;
      case "tool_proposed": next.status = `Preparing ${event.payload.tool_name}…`; break;
      case "tool_started": next.status = "Running an approved tool…"; break;
      case "permission_requested": next.status = `Waiting for approval: ${event.payload.tool_name}`; break;
      case "paused": next.status = "Background turn paused."; break;
      case "cancelling": next.status = "Stopping background turn…"; break;
      case "completed": next.summary = event.payload.summary; break;
      case "failed": next.error = event.payload.message; break;
      case "cancelled": next.status = event.payload.reason || "Background turn stopped."; break;
      case "needs_reconciliation": next.error = event.payload.reason; break;
      default: break;
    }
  }
  if (run?.status && ["succeeded", "failed", "cancelled", "needs_reconciliation"].includes(run.status)) {
    next.terminal = true;
    next.terminalStatus = run.status;
  }
  return next;
}

function storage(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}

export function loadActiveDaemonTurns(): ActiveDaemonDesktopTurn[] {
  const raw = storage()?.getItem(ACTIVE_TURNS_KEY);
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((entry): entry is ActiveDaemonDesktopTurn => {
      if (!entry || typeof entry !== "object") return false;
      const value = entry as Record<string, unknown>;
      return typeof value.sessionId === "string"
        && typeof value.turnId === "string"
        && typeof value.runId === "string"
        && Number.isSafeInteger(value.assistantIndex)
        && Number.isSafeInteger(value.lastSequence)
        && typeof value.output === "string";
    });
  } catch {
    return [];
  }
}

export function saveActiveDaemonTurn(link: ActiveDaemonDesktopTurn): void {
  const entries = loadActiveDaemonTurns().filter((entry) => entry.runId !== link.runId);
  entries.push(link);
  storage()?.setItem(ACTIVE_TURNS_KEY, JSON.stringify(entries));
}

export function removeActiveDaemonTurn(runId: string): void {
  storage()?.setItem(ACTIVE_TURNS_KEY, JSON.stringify(loadActiveDaemonTurns().filter((entry) => entry.runId !== runId)));
}

function wait(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * How long the UI waits for the durable policy to settle a turn's
 * workspace-mutation contract before showing what it already has.
 *
 * The daemon settles a contract on its own tick, so the decision arrives shortly
 * after the run ends rather than with it. Bounded because this is a wait for a
 * *display* — the contract is settled and any correction is submitted whether or
 * not anything is watching, so timing out here loses nothing but the live view.
 */
const CONTRACT_SETTLE_TIMEOUT_MS = 60_000;

/** What the durable backend says should happen after one attempt ends. */
interface DurableFollowUp {
  /** A continuation the backend submitted. The UI watches it; it never starts
   * it — see the ownership boundary in `agentLoop.ts`. */
  runId?: string;
  /** The contract's own verdict, to be shown in place of an answer that claimed
   * a change it did not make. */
  failure?: string;
}

/**
 * Ask the durable backend what became of this turn once an attempt ended.
 *
 * This is the whole of the UI's part in the workspace-mutation contract: it
 * reads. Whether a correction was needed, whether one was submitted, and what it
 * runs under were all decided by the policy against durable state.
 */
async function durableFollowUp(
  link: ActiveDaemonDesktopTurn,
  watched: ReadonlySet<string>,
): Promise<DurableFollowUp | null> {
  const deadline = Date.now() + CONTRACT_SETTLE_TIMEOUT_MS;
  for (;;) {
    const detail = await ingressTurnShow(link.source ?? "desktop", link.sessionId, link.turnId)
      .catch(() => null);
    const turn = detail?.turn ?? null;
    // No durable row (an older daemon, or a turn that never reached ingress)
    // and nothing that promised a change: there is nothing to follow.
    if (!turn || !turn.mutation_required) return null;
    const unwatched = (detail?.continuations ?? []).filter(
      (child) => child.run_id !== null && !watched.has(child.run_id),
    );
    const continuation = unwatched[unwatched.length - 1];
    if (continuation?.run_id) return { runId: continuation.run_id };
    if (turn.mutation_state === "unmet" || turn.mutation_state === "interrupted") {
      return { failure: turn.mutation_detail ?? undefined };
    }
    if (turn.mutation_state !== null) return null;
    if (Date.now() >= deadline) return null;
    await wait(POLL_INTERVAL_MS);
  }
}

export async function watchDaemonDesktopTurn(
  initialLink: ActiveDaemonDesktopTurn,
  signal: AbortSignal,
  callbacks: WatchDaemonTurnCallbacks,
): Promise<DaemonTurnProjection> {
  let link = { ...initialLink };
  let projection = emptyProjection(link);
  const watched = new Set<string>([link.runId]);
  let cancelSent = false;
  const requestCancel = () => {
    if (cancelSent) return;
    cancelSent = true;
    void daemonCancel(link.runId, "Stopped from desktop chat").catch(() => undefined);
  };
  signal.addEventListener("abort", requestCancel, { once: true });
  if (signal.aborted) requestCancel();
  try {
    for (;;) {
      const [events, run] = await Promise.all([
        loadRunEvents(link.runId, link.lastSequence, 1_000),
        getRun(link.runId),
      ]);
      projection = projectDaemonTurnEvents(projection, events, run);
      link = { ...link, lastSequence: projection.lastSequence, output: projection.output };
      saveActiveDaemonTurn(link);
      callbacks.onLinkChanged?.(link);
      callbacks.onProjection(projection);
      if (projection.terminal) {
        // The operator asked to stop; a stopped turn is not corrected.
        if (signal.aborted) return projection;
        const followUp = await durableFollowUp(link, watched);
        if (followUp?.runId) {
          // The durable backend is running a continuation of this same accepted
          // turn. Its answer replaces the one that claimed a change it did not
          // make, exactly as the in-process loop used to discard that answer —
          // except that the attempt is a durable run this only watches.
          const finished = link.runId;
          watched.add(followUp.runId);
          link = { ...link, runId: followUp.runId, lastSequence: 0, output: "" };
          removeActiveDaemonTurn(finished);
          projection = {
            ...emptyProjection(link),
            status: "Making the requested workspace change…",
          };
          saveActiveDaemonTurn(link);
          callbacks.onLinkChanged?.(link);
          callbacks.onProjection(projection);
          continue;
        }
        if (followUp?.failure) {
          projection = {
            ...projection,
            output: "",
            error: followUp.failure,
            terminalStatus: "failed",
          };
          callbacks.onProjection(projection);
        }
        return projection;
      }
      await wait(POLL_INTERVAL_MS);
    }
  } finally {
    signal.removeEventListener("abort", requestCancel);
  }
}
