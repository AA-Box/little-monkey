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
  workspaceToRunWire,
} from "./durableRun";
import type { ModelTargetSnapshot } from "./modelTargets";
import type { PermissionMode } from "../store/permissionStore";
import type { WorkspaceRootInfo } from "../store/workspaceStore";
import type { McpServerInfo } from "../store/mcpStore";

export const DAEMON_DESKTOP_TURN_SCHEMA_VERSION = 2 as const;
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
  workspace: string;
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
    workspace: WorkspaceContextWire;
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
}

export interface ActiveDaemonDesktopTurn {
  sessionId: string;
  turnId: string;
  runId: string;
  assistantIndex: number;
  lastSequence: number;
  output: string;
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
  if (!workspace || !primary) {
    throw new Error("Daemon-backed turns require an open workspace snapshot.");
  }
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
    workspace: primary.path,
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
        root_id: root.id,
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
    },
  };
}

export function daemonRouteFromStatus(status: DaemonStatus): "fallback" | "daemon" {
  if (!status.installed) return "fallback";
  if (!status.serviceRunning || !status.heartbeatFresh) {
    throw new Error("The installed M6A resident runner is not healthy. Start it in Background Agents before sending this turn.");
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

/** Local browser/dev profiles that cannot expose the typed command are the
 * only error case treated as legacy fallback. An installed-but-broken daemon
 * never silently changes execution authority. */
export async function daemonDesktopRoute(): Promise<"fallback" | "daemon"> {
  if (!isTauri()) return "fallback";
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

export async function watchDaemonDesktopTurn(
  initialLink: ActiveDaemonDesktopTurn,
  signal: AbortSignal,
  callbacks: WatchDaemonTurnCallbacks,
): Promise<DaemonTurnProjection> {
  let link = { ...initialLink };
  let projection = emptyProjection(link);
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
      if (projection.terminal) return projection;
      await wait(POLL_INTERVAL_MS);
    }
  } finally {
    signal.removeEventListener("abort", requestCancel);
  }
}
