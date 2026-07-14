import type { ChatMessage, ToolCall } from "./llamaClient";
import { isModelTargetSnapshot, type ModelTargetSnapshot } from "./modelTargets";

export const MIN_CREW_MEMBERS = 2;
export const MAX_CREW_MEMBERS = 4;

/** Hard product ceilings. A saved run carries an exact copy so reload and
 * retry cannot silently inherit looser limits from a later app version. */
export const DEFAULT_CREW_LIMITS = Object.freeze({
  maxMembers: MAX_CREW_MEMBERS,
  maxRounds: 1,
  maxModelCalls: 10,
  maxDurationMs: 5 * 60_000,
  maxTotalTokens: 32_000,
  maxCompletionTokensPerCall: 2_048,
  maxEstimatedCostUsd: 2,
});

export interface CrewLimits {
  maxMembers: number;
  maxRounds: 1;
  maxModelCalls: number;
  maxDurationMs: number;
  maxTotalTokens: number;
  maxCompletionTokensPerCall: number;
  maxEstimatedCostUsd: number;
}

export type CrewContextPolicy = "prompt_only" | "shared_session";
export type CrewToolProfile = "read_only";
export type CrewActorKind = "coordinator" | "member";

export interface CrewActorDefinition {
  id: string;
  name: string;
  role: string;
  personaId: string | null;
  modelTarget: ModelTargetSnapshot;
  contextPolicy: CrewContextPolicy;
  toolProfile: CrewToolProfile;
}

export interface CrewDefinition {
  version: 1;
  id: string;
  name: string;
  coordinator: CrewActorDefinition;
  members: CrewActorDefinition[];
  createdAt: number;
  updatedAt: number;
}

export interface CrewPersonaSnapshot {
  id: string;
  name: string;
  content: string;
}

export type CrewActorStatus = "idle" | "running" | "completed" | "failed" | "cancelled";
export type CrewRunStatus = "idle" | "running" | "completed" | "failed" | "cancelled";

export interface CrewUsage {
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
}

export type CrewToolRequestStatus = "running" | "completed" | "failed" | "blocked" | "cancelled";

/** A tool request is nested under an actor and also carries actorId. This
 * redundancy is deliberate: exported/audited records retain attribution
 * even after being flattened. */
export interface CrewToolRequest {
  id: string;
  actorId: string;
  name: string;
  arguments: string;
  status: CrewToolRequestStatus;
  requestedAt: number;
  completedAt: number | null;
  result: string | null;
  permission: "not_required_read_only" | "not_requested_blocked";
}

export interface CrewPermissionAttribution {
  id: string;
  actorId: string;
  tool: string;
  status: "requested" | "approved" | "denied" | "cancelled";
  requestedAt: number;
  decidedAt: number | null;
}

export type CrewTranscriptKind = "model" | "tool_request" | "tool_result" | "notice";

export interface CrewTranscriptEntry {
  id: string;
  actorId: string;
  at: number;
  kind: CrewTranscriptKind;
  content: string;
  toolCall?: ToolCall;
}

export interface CrewMutationProposal {
  id: string;
  actorId: string;
  summary: string;
  details: string;
  sourceActorIds: string[];
  status: "proposed";
}

export interface CrewActorRun {
  actorId: string;
  kind: CrewActorKind;
  name: string;
  role: string;
  persona: CrewPersonaSnapshot | null;
  modelTarget: ModelTargetSnapshot;
  contextPolicy: CrewContextPolicy;
  toolProfile: CrewToolProfile;
  systemPrompt: string;
  status: CrewActorStatus;
  startedAt: number | null;
  completedAt: number | null;
  durationMs: number | null;
  error: string | null;
  rawOutput: string;
  report: string | null;
  transcript: CrewTranscriptEntry[];
  toolRequests: CrewToolRequest[];
  permissions: CrewPermissionAttribution[];
  mutationProposals: CrewMutationProposal[];
  usage: CrewUsage;
  modelCalls: number;
  estimatedCostUsd: number;
}

export interface CrewInputSnapshot {
  sourceSessionId: string;
  prompt: string;
  storedContent: ChatMessage["content"];
  wireContent: ChatMessage["content"];
  baseMessages: ChatMessage[];
  contextMessages: ChatMessage[];
  unresolvedReferences: string[];
  createdAt: number;
}

export interface CrewBudgetState {
  modelCalls: number;
  totalTokens: number;
  estimatedCostUsd: number;
  limitReason: "calls" | "time" | "tokens" | "cost" | null;
}

export interface CrewRun {
  version: 1;
  id: string;
  crewId: string;
  crewName: string;
  status: CrewRunStatus;
  createdAt: number;
  startedAt: number | null;
  completedAt: number | null;
  durationMs: number | null;
  error: string | null;
  round: 0 | 1;
  limits: CrewLimits;
  budget: CrewBudgetState;
  input: CrewInputSnapshot;
  coordinator: CrewActorRun;
  members: CrewActorRun[];
  finalAnswer: string;
  mutationProposals: CrewMutationProposal[];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.trim().length > 0;
}

function finiteNonNegative(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

function nullableNumber(value: unknown): number | null {
  return finiteNonNegative(value) ? value : null;
}

function normalizeMessage(raw: unknown): ChatMessage | null {
  if (!isRecord(raw)) return null;
  if (!(["system", "user", "assistant", "tool"] as unknown[]).includes(raw.role)) return null;
  if (typeof raw.content !== "string" && !Array.isArray(raw.content)) return null;
  return structuredClone(raw) as unknown as ChatMessage;
}

function normalizeContent(raw: unknown): ChatMessage["content"] | null {
  if (typeof raw === "string") return raw;
  if (!Array.isArray(raw)) return null;
  const valid = raw.every((part) => {
    if (!isRecord(part)) return false;
    if (part.type === "text") return typeof part.text === "string";
    if (part.type !== "image_url" || !isRecord(part.image_url)) return false;
    return typeof part.image_url.url === "string";
  });
  return valid ? (structuredClone(raw) as ChatMessage["content"]) : null;
}

function normalizeActorDefinition(raw: unknown): CrewActorDefinition | null {
  if (!isRecord(raw)) return null;
  if (
    !nonEmptyString(raw.id) ||
    !nonEmptyString(raw.name) ||
    !nonEmptyString(raw.role) ||
    !(raw.personaId === null || nonEmptyString(raw.personaId)) ||
    !isModelTargetSnapshot(raw.modelTarget) ||
    (raw.contextPolicy !== "prompt_only" && raw.contextPolicy !== "shared_session") ||
    raw.toolProfile !== "read_only"
  ) {
    return null;
  }
  return {
    id: raw.id,
    name: raw.name.trim(),
    role: raw.role.trim(),
    personaId: raw.personaId,
    modelTarget: structuredClone(raw.modelTarget),
    contextPolicy: raw.contextPolicy,
    toolProfile: "read_only",
  };
}

export function normalizeCrewDefinition(raw: unknown): CrewDefinition | null {
  if (!isRecord(raw) || raw.version !== 1 || !nonEmptyString(raw.id) || !nonEmptyString(raw.name)) return null;
  const coordinator = normalizeActorDefinition(raw.coordinator);
  if (!coordinator || !Array.isArray(raw.members)) return null;
  const members = raw.members.map(normalizeActorDefinition).filter((actor): actor is CrewActorDefinition => actor !== null);
  if (members.length < MIN_CREW_MEMBERS || members.length > MAX_CREW_MEMBERS || members.length !== raw.members.length) {
    return null;
  }
  const actorIds = [coordinator.id, ...members.map((member) => member.id)];
  if (new Set(actorIds).size !== actorIds.length || members.some((member) => member.personaId === null)) return null;
  return {
    version: 1,
    id: raw.id,
    name: raw.name.trim(),
    coordinator,
    members,
    createdAt: finiteNonNegative(raw.createdAt) ? raw.createdAt : 0,
    updatedAt: finiteNonNegative(raw.updatedAt) ? raw.updatedAt : 0,
  };
}

export function isCrewDefinition(raw: unknown): raw is CrewDefinition {
  return normalizeCrewDefinition(raw) !== null;
}

function normalizeUsage(raw: unknown): CrewUsage {
  if (!isRecord(raw)) return { promptTokens: 0, completionTokens: 0, totalTokens: 0 };
  return {
    promptTokens: finiteNonNegative(raw.promptTokens) ? raw.promptTokens : 0,
    completionTokens: finiteNonNegative(raw.completionTokens) ? raw.completionTokens : 0,
    totalTokens: finiteNonNegative(raw.totalTokens) ? raw.totalTokens : 0,
  };
}

function normalizePersona(raw: unknown): CrewPersonaSnapshot | null {
  if (!isRecord(raw) || !nonEmptyString(raw.id) || !nonEmptyString(raw.name) || typeof raw.content !== "string") return null;
  return { id: raw.id, name: raw.name, content: raw.content };
}

function normalizeMutation(raw: unknown): CrewMutationProposal | null {
  if (!isRecord(raw) || !nonEmptyString(raw.id) || !nonEmptyString(raw.actorId) || !nonEmptyString(raw.summary)) return null;
  return {
    id: raw.id,
    actorId: raw.actorId,
    summary: raw.summary,
    details: typeof raw.details === "string" ? raw.details : "",
    sourceActorIds: Array.isArray(raw.sourceActorIds)
      ? raw.sourceActorIds.filter((id): id is string => nonEmptyString(id))
      : [],
    status: "proposed",
  };
}

function normalizeTranscript(raw: unknown, actorId: string): CrewTranscriptEntry[] {
  if (!Array.isArray(raw)) return [];
  return raw.flatMap((entry): CrewTranscriptEntry[] => {
    if (!isRecord(entry) || !nonEmptyString(entry.id) || typeof entry.content !== "string") return [];
    if (!(entry.kind === "model" || entry.kind === "tool_request" || entry.kind === "tool_result" || entry.kind === "notice")) return [];
    return [{
      id: entry.id,
      actorId,
      at: finiteNonNegative(entry.at) ? entry.at : 0,
      kind: entry.kind,
      content: entry.content,
      ...(isRecord(entry.toolCall) ? { toolCall: structuredClone(entry.toolCall) as unknown as ToolCall } : {}),
    }];
  });
}

function normalizeToolRequests(raw: unknown, actorId: string): CrewToolRequest[] {
  if (!Array.isArray(raw)) return [];
  return raw.flatMap((entry): CrewToolRequest[] => {
    if (!isRecord(entry) || !nonEmptyString(entry.id) || !nonEmptyString(entry.name) || typeof entry.arguments !== "string") return [];
    const validStatuses: CrewToolRequestStatus[] = ["running", "completed", "failed", "blocked", "cancelled"];
    const status = validStatuses.includes(entry.status as CrewToolRequestStatus) ? entry.status as CrewToolRequestStatus : "failed";
    return [{
      id: entry.id,
      actorId,
      name: entry.name,
      arguments: entry.arguments,
      status,
      requestedAt: finiteNonNegative(entry.requestedAt) ? entry.requestedAt : 0,
      completedAt: nullableNumber(entry.completedAt),
      result: typeof entry.result === "string" ? entry.result : null,
      permission: entry.permission === "not_required_read_only" ? "not_required_read_only" : "not_requested_blocked",
    }];
  });
}

function normalizePermissions(raw: unknown, actorId: string): CrewPermissionAttribution[] {
  if (!Array.isArray(raw)) return [];
  return raw.flatMap((entry): CrewPermissionAttribution[] => {
    if (!isRecord(entry) || !nonEmptyString(entry.id) || !nonEmptyString(entry.tool)) return [];
    if (!(entry.status === "requested" || entry.status === "approved" || entry.status === "denied" || entry.status === "cancelled")) return [];
    return [{
      id: entry.id,
      actorId,
      tool: entry.tool,
      status: entry.status,
      requestedAt: finiteNonNegative(entry.requestedAt) ? entry.requestedAt : 0,
      decidedAt: nullableNumber(entry.decidedAt),
    }];
  });
}

function normalizeActorRun(raw: unknown, kind: CrewActorKind, interruptRunning: boolean): CrewActorRun | null {
  if (!isRecord(raw) || !nonEmptyString(raw.actorId) || !nonEmptyString(raw.name) || !nonEmptyString(raw.role)) return null;
  if (!isModelTargetSnapshot(raw.modelTarget) || typeof raw.systemPrompt !== "string") return null;
  if ((raw.contextPolicy !== "prompt_only" && raw.contextPolicy !== "shared_session") || raw.toolProfile !== "read_only") return null;
  const validStatuses: CrewActorStatus[] = ["idle", "running", "completed", "failed", "cancelled"];
  let status = validStatuses.includes(raw.status as CrewActorStatus) ? raw.status as CrewActorStatus : "idle";
  const interrupted = interruptRunning && status === "running";
  if (interrupted) status = "failed";
  const actorId = raw.actorId;
  return {
    actorId,
    kind,
    name: raw.name,
    role: raw.role,
    persona: raw.persona === null ? null : normalizePersona(raw.persona),
    modelTarget: structuredClone(raw.modelTarget),
    contextPolicy: raw.contextPolicy,
    toolProfile: "read_only",
    systemPrompt: raw.systemPrompt,
    status,
    startedAt: nullableNumber(raw.startedAt),
    completedAt: nullableNumber(raw.completedAt),
    durationMs: nullableNumber(raw.durationMs),
    error: interrupted
      ? "Interrupted when Little Monkey closed. Retry the Crew from its frozen input."
      : typeof raw.error === "string" ? raw.error : null,
    rawOutput: typeof raw.rawOutput === "string" ? raw.rawOutput : "",
    report: typeof raw.report === "string" ? raw.report : null,
    transcript: normalizeTranscript(raw.transcript, actorId),
    toolRequests: normalizeToolRequests(raw.toolRequests, actorId),
    permissions: normalizePermissions(raw.permissions, actorId),
    mutationProposals: Array.isArray(raw.mutationProposals)
      ? raw.mutationProposals.map(normalizeMutation).filter((item): item is CrewMutationProposal => item !== null)
      : [],
    usage: normalizeUsage(raw.usage),
    modelCalls: finiteNonNegative(raw.modelCalls) ? raw.modelCalls : 0,
    estimatedCostUsd: finiteNonNegative(raw.estimatedCostUsd) ? raw.estimatedCostUsd : 0,
  };
}

function normalizeLimits(raw: unknown): CrewLimits {
  if (!isRecord(raw)) return { ...DEFAULT_CREW_LIMITS };
  return {
    maxMembers: MAX_CREW_MEMBERS,
    maxRounds: 1,
    maxModelCalls: finiteNonNegative(raw.maxModelCalls) ? Math.min(raw.maxModelCalls, DEFAULT_CREW_LIMITS.maxModelCalls) : DEFAULT_CREW_LIMITS.maxModelCalls,
    maxDurationMs: finiteNonNegative(raw.maxDurationMs) ? Math.min(raw.maxDurationMs, DEFAULT_CREW_LIMITS.maxDurationMs) : DEFAULT_CREW_LIMITS.maxDurationMs,
    maxTotalTokens: finiteNonNegative(raw.maxTotalTokens) ? Math.min(raw.maxTotalTokens, DEFAULT_CREW_LIMITS.maxTotalTokens) : DEFAULT_CREW_LIMITS.maxTotalTokens,
    maxCompletionTokensPerCall: finiteNonNegative(raw.maxCompletionTokensPerCall)
      ? Math.min(raw.maxCompletionTokensPerCall, DEFAULT_CREW_LIMITS.maxCompletionTokensPerCall)
      : DEFAULT_CREW_LIMITS.maxCompletionTokensPerCall,
    maxEstimatedCostUsd: finiteNonNegative(raw.maxEstimatedCostUsd)
      ? Math.min(raw.maxEstimatedCostUsd, DEFAULT_CREW_LIMITS.maxEstimatedCostUsd)
      : DEFAULT_CREW_LIMITS.maxEstimatedCostUsd,
  };
}

export function normalizeCrewRun(raw: unknown, interruptRunning: boolean): CrewRun | null {
  if (!isRecord(raw) || raw.version !== 1 || !nonEmptyString(raw.id) || !nonEmptyString(raw.crewId) || !nonEmptyString(raw.crewName)) return null;
  if (!isRecord(raw.input) || !Array.isArray(raw.members)) return null;
  const storedContent = normalizeContent(raw.input.storedContent);
  const wireContent = normalizeContent(raw.input.wireContent);
  if (storedContent === null || wireContent === null || !nonEmptyString(raw.input.sourceSessionId) || !nonEmptyString(raw.input.prompt)) return null;
  const coordinator = normalizeActorRun(raw.coordinator, "coordinator", interruptRunning);
  const members = raw.members.map((member) => normalizeActorRun(member, "member", interruptRunning)).filter((actor): actor is CrewActorRun => actor !== null);
  if (!coordinator || members.length < MIN_CREW_MEMBERS || members.length > MAX_CREW_MEMBERS || members.length !== raw.members.length) return null;
  const validStatuses: CrewRunStatus[] = ["idle", "running", "completed", "failed", "cancelled"];
  let status = validStatuses.includes(raw.status as CrewRunStatus) ? raw.status as CrewRunStatus : "idle";
  const interrupted = interruptRunning && status === "running";
  if (interrupted) status = "failed";
  const budgetRaw = isRecord(raw.budget) ? raw.budget : {};
  return {
    version: 1,
    id: raw.id,
    crewId: raw.crewId,
    crewName: raw.crewName,
    status,
    createdAt: finiteNonNegative(raw.createdAt) ? raw.createdAt : 0,
    startedAt: nullableNumber(raw.startedAt),
    completedAt: nullableNumber(raw.completedAt),
    durationMs: nullableNumber(raw.durationMs),
    error: interrupted
      ? "Interrupted when Little Monkey closed. Retry the Crew from its frozen input."
      : typeof raw.error === "string" ? raw.error : null,
    round: raw.round === 1 ? 1 : 0,
    limits: normalizeLimits(raw.limits),
    budget: {
      modelCalls: finiteNonNegative(budgetRaw.modelCalls) ? budgetRaw.modelCalls : 0,
      totalTokens: finiteNonNegative(budgetRaw.totalTokens) ? budgetRaw.totalTokens : 0,
      estimatedCostUsd: finiteNonNegative(budgetRaw.estimatedCostUsd) ? budgetRaw.estimatedCostUsd : 0,
      limitReason: budgetRaw.limitReason === "calls" || budgetRaw.limitReason === "time" || budgetRaw.limitReason === "tokens" || budgetRaw.limitReason === "cost"
        ? budgetRaw.limitReason : null,
    },
    input: {
      sourceSessionId: raw.input.sourceSessionId,
      prompt: raw.input.prompt,
      storedContent,
      wireContent,
      baseMessages: Array.isArray(raw.input.baseMessages)
        ? raw.input.baseMessages.map(normalizeMessage).filter((message): message is ChatMessage => message !== null)
        : [],
      contextMessages: Array.isArray(raw.input.contextMessages)
        ? raw.input.contextMessages.map(normalizeMessage).filter((message): message is ChatMessage => message !== null)
        : [],
      unresolvedReferences: Array.isArray(raw.input.unresolvedReferences)
        ? raw.input.unresolvedReferences.filter((item): item is string => typeof item === "string")
        : [],
      createdAt: finiteNonNegative(raw.input.createdAt) ? raw.input.createdAt : 0,
    },
    coordinator,
    members,
    finalAnswer: typeof raw.finalAnswer === "string" ? raw.finalAnswer : "",
    mutationProposals: Array.isArray(raw.mutationProposals)
      ? raw.mutationProposals.map(normalizeMutation).filter((item): item is CrewMutationProposal => item !== null)
      : [],
  };
}

export function emptyCrewUsage(): CrewUsage {
  return { promptTokens: 0, completionTokens: 0, totalTokens: 0 };
}

