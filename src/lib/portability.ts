import { invoke, isTauri } from "@tauri-apps/api/core";

import { textContent, type ChatContentPart, type ChatMessage } from "./llamaClient";
import { isModelTargetSnapshot, type ModelTargetSnapshot } from "./modelTargets";
import {
  applyPortableSessionImportPlan,
  planPortableSessionImport,
  portableSessionPlanPayload,
  useSessionStore,
  type ChatSession,
  type MessageTranslation,
  type SessionGroup,
  type ThreadTranslation,
} from "../store/sessionStore";
import type { CrewDefinition } from "./crewTypes";
import {
  applyPortablePromptImportPlan,
  planPortablePromptImport,
  portablePromptPlanPayload,
  usePromptStore,
  type PromptEntry,
} from "../store/promptStore";
import { useStackStore, type KnowledgeStack } from "../store/stackStore";
import { LOCALE_STORAGE_KEY, useLocaleStore } from "../store/localeStore";
import { useShortcutStore } from "../store/shortcutStore";
import {
  hydrateShortcutOverrides,
  SHORTCUT_STORAGE_KEY,
  SHORTCUT_STORAGE_VERSION,
} from "../store/shortcutStore";
import { LOCALES, type LocaleCode } from "./i18n";

export type PortableContentBlock =
  | { type: "text"; text: string }
  | { type: "code"; language: string | null; code: string }
  | { type: "table"; headers: string[]; rows: string[][] };

export interface PortableMessageTranslation {
  locale: string;
  originalBlocks: PortableContentBlock[];
  translatedBlocks: PortableContentBlock[];
  sourceSha256: string;
  createdAtMs: number;
  metadata: Record<string, unknown>;
}

export interface PortableThreadTranslation {
  locale: string;
  originalTitle: string;
  translatedTitle: string;
  sourceSha256: string;
  translatedMessageIds: string[];
  createdAtMs: number;
  metadata: Record<string, unknown>;
}

export interface PortableMessage {
  id: string;
  role: string;
  ordinal: number;
  createdAtMs: number;
  blocks: PortableContentBlock[];
  attachmentIds: string[];
  externalReferences: string[];
  translations: PortableMessageTranslation[];
  metadata: Record<string, unknown>;
}

export interface PortableSession {
  id: string;
  title: string;
  ordinal: number;
  createdAtMs: number;
  updatedAtMs: number;
  archived: boolean;
  pinned: boolean;
  modelKey: string | null;
  personaId: string | null;
  workspacePath: string | null;
  messages: PortableMessage[];
  translations: PortableThreadTranslation[];
  metadata: Record<string, unknown>;
}

export interface PortableDataV1 {
  schemaVersion: 1;
  sessions: PortableSession[];
  metadata: Record<string, unknown>;
}

export interface PortableArtifactRequest {
  id: string;
  mediaType: string;
  bytesBase64: string;
}

export interface PortableBundleRequest {
  bundleId: string;
  exportedAtMs: number;
  appVersion: string;
  data: PortableDataV1;
  artifacts: PortableArtifactRequest[];
}

export interface PortablePreflightReport {
  archiveSha256: string;
  entryCount: number;
  compressedBytes: number;
  expandedBytes: number;
  sessionCount: number;
  messageCount: number;
  artifactCount: number;
  externalReferenceCount: number;
}

export interface PortableArtifactResponse {
  id: string;
  mediaType: string;
  bytesBase64: string;
}

export interface PortableReadOutcome {
  data: PortableDataV1;
  artifacts: PortableArtifactResponse[];
  preflight: PortablePreflightReport;
}

type PortableShortcutOverrides = ReturnType<typeof hydrateShortcutOverrides>;

interface PortableRestoreSettings {
  locale: string | null;
  shortcutOverrides: PortableShortcutOverrides | null;
}

interface PortableRestoreCommandOutcome {
  transactionId: string;
  stacks: KnowledgeStack[];
  profileCounts: {
    groups: number;
    sessions: number;
    messages: number;
    actorTranscripts: number;
    crews: number;
    attachmentOccurrences: number;
    uniqueArtifacts: number;
  };
  settingsPending: boolean;
}

export interface PendingPortableRestoreSettings {
  schemaVersion: 1;
  transactionId: string;
  locale: string | null;
  shortcutOverrides: unknown;
}

export interface SnapshotRetentionPolicy {
  maxCount: number;
  maxTotalBytes: number;
  maxAgeMs: number | null;
}

export interface SnapshotFileInfo {
  path: string;
  createdAtMs: number;
  byteSize: number;
  sha256: string;
}

export interface SnapshotWriteOutcome {
  snapshot: SnapshotFileInfo;
  alreadyExisted: boolean;
  pruned: string[];
}

export interface WebDavBackupConfig {
  enabled: boolean;
  baseUrl: string;
  username: string;
  remotePath: string;
  deviceId: string;
  intervalMinutes: number;
  knownEtag: string | null;
  lastAttemptMs: number | null;
  lastSuccessMs: number | null;
  nextDueMs: number | null;
  lastUploadedSha256: string | null;
  lastUploadedRemotePath: string | null;
  lastError: string | null;
  consecutiveFailures: number;
}

export interface WebDavStagedSnapshot {
  path: string;
  createdAtMs: number;
  byteSize: number;
  sha256: string;
  sourceRevisionSha256: string;
}

export interface WebDavBackupStatus {
  config: WebDavBackupConfig;
  stagedSnapshot: WebDavStagedSnapshot | null;
  credentialsAvailable: boolean;
  uploadClaimed: boolean;
  claimOwner: string | null;
  claimExpiresMs: number | null;
  ready: boolean;
  readinessError: string | null;
}

export type WebDavBackgroundRunOutcome =
  | { status: "disabled" }
  | { status: "not_due"; nextDueMs: number }
  | { status: "missing_staged_source" }
  | { status: "busy"; owner: string; expiresAtMs: number }
  | { status: "already_current"; snapshotSha256: string; nextDueMs: number }
  | { status: "uploaded"; remotePath: string; etag: string; snapshotSha256: string }
  | {
      status: "conflict_copy";
      remotePath: string;
      etag: string;
      conflictingEtag: string | null;
      snapshotSha256: string;
    };

const SECRET_KEY_PARTS = [
  "credential",
  "apikey",
  "accesstoken",
  "refreshtoken",
  "authtoken",
  "authorization",
  "password",
  "privatekey",
  "clientsecret",
  "cookie",
  "sessiontoken",
];

function isSecretKey(key: string): boolean {
  const normalized = key.replace(/[^A-Za-z0-9]/g, "").toLowerCase();
  return SECRET_KEY_PARTS.some((part) => normalized.includes(part)) || normalized === "secret" || normalized.endsWith("secret");
}

/** Removes credential-bearing fields from portable metadata recursively.
 * Ordinary transcript strings are left intact; only schema keys that could
 * accidentally serialize a credential store are excluded. */
export function sanitizePortableMetadata(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sanitizePortableMetadata);
  if (!value || typeof value !== "object") return value;
  return Object.fromEntries(
    Object.entries(value as Record<string, unknown>)
      .filter(([key]) => !isSecretKey(key))
      .map(([key, child]) => [key, sanitizePortableMetadata(child)]),
  );
}

function canonicalize(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonicalize);
  if (!value || typeof value !== "object") return value;
  return Object.fromEntries(
    Object.entries(value as Record<string, unknown>)
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([key, child]) => [key, canonicalize(child)]),
  );
}

async function sha256Bytes(bytes: Uint8Array): Promise<string> {
  const input = new Uint8Array(bytes).buffer;
  const digest = await crypto.subtle.digest("SHA-256", input);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function sha256Text(text: string): Promise<string> {
  return sha256Bytes(new TextEncoder().encode(text));
}

function bytesToBase64(bytes: Uint8Array): string {
  let output = "";
  for (let offset = 0; offset < bytes.length; offset += 0x8000) {
    output += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
  }
  return btoa(output);
}

function base64ToBytes(value: string): Uint8Array {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return bytes;
}

function parseTable(lines: string[], start: number): { block: PortableContentBlock; next: number } | null {
  const header = lines[start];
  const separator = lines[start + 1];
  if (!header?.includes("|") || !separator || !/^\s*\|?\s*:?-{3,}:?\s*(?:\|\s*:?-{3,}:?\s*)+\|?\s*$/.test(separator)) {
    return null;
  }
  const cells = (line: string) => line.trim().replace(/^\||\|$/g, "").split("|").map((cell) => cell.trim());
  const headers = cells(header);
  const rows: string[][] = [];
  let cursor = start + 2;
  while (cursor < lines.length && lines[cursor].includes("|") && lines[cursor].trim()) {
    const row = cells(lines[cursor]);
    if (row.length !== headers.length) break;
    rows.push(row);
    cursor += 1;
  }
  return { block: { type: "table", headers, rows }, next: cursor };
}

function splitTextAndTables(text: string): PortableContentBlock[] {
  if (!text) return [];
  const lines = text.split("\n");
  const blocks: PortableContentBlock[] = [];
  let plainStart = 0;
  let index = 0;
  while (index < lines.length) {
    const table = parseTable(lines, index);
    if (!table) {
      index += 1;
      continue;
    }
    if (index > plainStart) blocks.push({ type: "text", text: lines.slice(plainStart, index).join("\n") });
    blocks.push(table.block);
    index = table.next;
    plainStart = index;
  }
  if (plainStart < lines.length) blocks.push({ type: "text", text: lines.slice(plainStart).join("\n") });
  return blocks;
}

/** Location-preserving Markdown split used by Markdown/Word export. */
export function contentBlocks(text: string): PortableContentBlock[] {
  const blocks: PortableContentBlock[] = [];
  const fence = /```([^\n`]*)\n([\s\S]*?)```/g;
  let cursor = 0;
  for (let match = fence.exec(text); match; match = fence.exec(text)) {
    blocks.push(...splitTextAndTables(text.slice(cursor, match.index)));
    blocks.push({
      type: "code",
      language: match[1].trim().split(/\s+/)[0] || null,
      code: match[2].replace(/\n$/, ""),
    });
    cursor = match.index + match[0].length;
  }
  blocks.push(...splitTextAndTables(text.slice(cursor)));
  return blocks.length > 0 ? blocks : [{ type: "text", text: "" }];
}

function blocksToMarkdown(blocks: readonly PortableContentBlock[]): string {
  return blocks.map((block) => {
    if (block.type === "text") return block.text;
    if (block.type === "code") return `\`\`\`${block.language ?? ""}\n${block.code}\n\`\`\``;
    const header = `| ${block.headers.join(" | ")} |`;
    const separator = `| ${block.headers.map(() => "---").join(" | ")} |`;
    const rows = block.rows.map((row) => `| ${row.join(" | ")} |`);
    return [header, separator, ...rows].join("\n");
  }).join("");
}

async function translationRecord(
  translation: MessageTranslation,
  originalBlocks: PortableContentBlock[],
): Promise<PortableMessageTranslation> {
  return {
    locale: translation.locale,
    originalBlocks,
    translatedBlocks: contentBlocks(translation.translatedText),
    sourceSha256: await sha256Text(JSON.stringify(canonicalize(originalBlocks))),
    createdAtMs: Math.max(1, Math.trunc(translation.createdAt)),
    metadata: sanitizePortableMetadata({ modelTarget: translation.modelTarget }) as Record<string, unknown>,
  };
}

async function artifactFromDataUrl(url: string): Promise<PortableArtifactRequest | null> {
  const match = /^data:([^;,]+);base64,([A-Za-z0-9+/=\s]+)$/.exec(url);
  if (!match) return null;
  const bytes = base64ToBytes(match[2].replace(/\s/g, ""));
  return {
    id: await sha256Bytes(bytes),
    mediaType: match[1],
    bytesBase64: bytesToBase64(bytes),
  };
}

async function portableMessage(
  session: ChatSession,
  message: ChatMessage,
  index: number,
  artifacts: Map<string, PortableArtifactRequest>,
): Promise<PortableMessage> {
  const blocks = contentBlocks(textContent(message.content));
  const attachmentIds: string[] = [];
  const externalReferences: string[] = [];
  if (Array.isArray(message.content)) {
    for (const part of message.content) {
      if (part.type !== "image_url") continue;
      const artifact = await artifactFromDataUrl(part.image_url.url);
      if (artifact) {
        artifacts.set(artifact.id, artifact);
        attachmentIds.push(artifact.id);
      } else if (/^https?:\/\//i.test(part.image_url.url)) {
        externalReferences.push(part.image_url.url);
      }
    }
  }
  const latestByLocale = new Map<string, MessageTranslation>();
  for (const translation of session.messageTranslations ?? []) {
    if (translation.messageIndex !== index || JSON.stringify(translation.originalContent) !== JSON.stringify(message.content)) continue;
    const key = translation.locale.toLowerCase();
    if ((latestByLocale.get(key)?.createdAt ?? 0) <= translation.createdAt) latestByLocale.set(key, translation);
  }
  const translations = await Promise.all([...latestByLocale.values()].map((translation) => translationRecord(translation, blocks)));
  const { content: _content, ...messageMetadata } = message;
  return {
    id: `message-${session.id}-${index}`,
    role: message.role,
    ordinal: index,
    createdAtMs: Math.max(1, Math.trunc(session.createdAt) + index),
    blocks,
    attachmentIds,
    externalReferences,
    translations,
    metadata: sanitizePortableMetadata({ chat: messageMetadata }) as Record<string, unknown>,
  };
}

async function portableThreadTranslations(
  session: ChatSession,
  messages: readonly PortableMessage[],
): Promise<PortableThreadTranslation[]> {
  const latestByLocale = new Map<string, ThreadTranslation>();
  for (const translation of session.threadTranslations ?? []) {
    if (translation.originalTitle !== session.title) continue;
    const key = translation.locale.toLowerCase();
    if ((latestByLocale.get(key)?.createdAt ?? 0) <= translation.createdAt) latestByLocale.set(key, translation);
  }
  const titleDigest = await sha256Text(session.title);
  return [...latestByLocale.values()].map((translation) => ({
    locale: translation.locale,
    originalTitle: session.title,
    translatedTitle: translation.translatedTitle,
    sourceSha256: titleDigest,
    translatedMessageIds: translation.translatedMessageIndices
      .map((index) => messages[index]?.id)
      .filter((id): id is string => Boolean(id)),
    createdAtMs: Math.max(1, Math.trunc(translation.createdAt)),
    metadata: sanitizePortableMetadata({ modelTarget: translation.modelTarget }) as Record<string, unknown>,
  }));
}

export async function portableSession(
  session: ChatSession,
  ordinal = 0,
  artifacts = new Map<string, PortableArtifactRequest>(),
): Promise<PortableSession> {
  const messages = await Promise.all(session.messages.map((message, index) => portableMessage(session, message, index, artifacts)));
  const {
    id: _id,
    title: _title,
    messages: _messages,
    createdAt: _createdAt,
    updatedAt: _updatedAt,
    archived: _archived,
    pinned: _pinned,
    personaId: _personaId,
    workspacePath: _workspacePath,
    messageTranslations: _messageTranslations,
    threadTranslations: _threadTranslations,
    ...sessionMetadata
  } = session;
  return {
    id: session.id,
    title: session.title,
    ordinal,
    createdAtMs: Math.max(1, Math.trunc(session.createdAt)),
    updatedAtMs: Math.max(1, Math.trunc(session.updatedAt)),
    archived: session.archived,
    pinned: session.pinned,
    modelKey: session.modelTarget?.key ?? null,
    personaId: session.personaId,
    workspacePath: session.workspacePath,
    messages,
    translations: await portableThreadTranslations(session, messages),
    metadata: sanitizePortableMetadata({ session: sessionMetadata }) as Record<string, unknown>,
  };
}

export async function buildPortableBundleRequest(sessionIds?: readonly string[]): Promise<PortableBundleRequest> {
  const sessionState = useSessionStore.getState();
  const selected = sessionIds
    ? sessionState.sessions.filter((session) => sessionIds.includes(session.id))
    : sessionState.sessions;
  if (selected.length === 0) throw new Error("There are no conversations to export.");
  const artifacts = new Map<string, PortableArtifactRequest>();
  const sessions: PortableSession[] = [];
  for (let index = 0; index < selected.length; index += 1) {
    sessions.push(await portableSession(selected[index], index, artifacts));
  }
  const promptState = usePromptStore.getState();
  const stackState = useStackStore.getState();
  const exportedAtMs = Date.now();
  return {
    bundleId: `bundle-${crypto.randomUUID()}`,
    exportedAtMs,
    appVersion: "0.1.0",
    data: {
      schemaVersion: 1,
      sessions,
      metadata: sanitizePortableMetadata({
        profileSchemaVersion: 1,
        groups: sessionState.groups,
        crews: sessionState.crews,
        prompts: {
          entries: promptState.entries,
          defaultPersonaId: promptState.defaultPersonaId,
        },
        stackDefinitions: stackState.stacks.map((stack) => ({ ...stack, indexed_at: null, chunk_count: 0 })),
        settings: {
          locale: useLocaleStore.getState().locale,
          shortcutOverrides: useShortcutStore.getState().overrides,
        },
      }) as Record<string, unknown>,
    },
    artifacts: [...artifacts.values()].sort((left, right) => left.id.localeCompare(right.id)),
  };
}

function restoreModelTarget(value: unknown): ModelTargetSnapshot | null {
  if (!value || typeof value !== "object") return null;
  const candidate = structuredClone(value) as Record<string, unknown>;
  if (candidate.kind === "provider" && typeof candidate.providerId === "string") {
    candidate.credentialRefId = `keychain:com.littlemonkey.app:${candidate.providerId}`;
  }
  return isModelTargetSnapshot(candidate) ? candidate : null;
}

function artifactContent(
  ids: readonly string[],
  externalReferences: readonly string[],
  artifacts: ReadonlyMap<string, PortableArtifactResponse>,
): ChatContentPart[] {
  const parts: ChatContentPart[] = [];
  for (const id of ids) {
    const artifact = artifacts.get(id);
    if (artifact?.mediaType.startsWith("image/")) {
      parts.push({ type: "image_url", image_url: { url: `data:${artifact.mediaType};base64,${artifact.bytesBase64}` } });
    }
  }
  for (const url of externalReferences) parts.push({ type: "image_url", image_url: { url } });
  return parts;
}

function reconstructedMessage(message: PortableMessage, artifacts: ReadonlyMap<string, PortableArtifactResponse>): ChatMessage {
  const text = blocksToMarkdown(message.blocks);
  const imageParts = artifactContent(message.attachmentIds, message.externalReferences, artifacts);
  const metadata = message.metadata.chat && typeof message.metadata.chat === "object"
    ? structuredClone(message.metadata.chat) as Partial<ChatMessage>
    : {};
  const content: ChatMessage["content"] = imageParts.length > 0
    ? [{ type: "text", text }, ...imageParts]
    : text;
  return { ...metadata, role: message.role as ChatMessage["role"], content } as ChatMessage;
}

function reconstructedSession(session: PortableSession, artifacts: ReadonlyMap<string, PortableArtifactResponse>): ChatSession {
  const metadata = session.metadata.session && typeof session.metadata.session === "object"
    ? structuredClone(session.metadata.session) as Partial<ChatSession>
    : {};
  const messages = session.messages.map((message) => reconstructedMessage(message, artifacts));
  const modelTarget = restoreModelTarget(metadata.modelTarget);
  const messageTranslations: MessageTranslation[] = session.messages.flatMap((message, messageIndex) =>
    message.translations.map((translation) => ({
      messageIndex,
      role: (message.role === "assistant" ? "assistant" : "user") as "user" | "assistant",
      locale: translation.locale,
      originalContent: structuredClone(messages[messageIndex].content),
      translatedText: blocksToMarkdown(translation.translatedBlocks),
      sourceSha256: translation.sourceSha256,
      createdAt: translation.createdAtMs,
      modelTarget: restoreModelTarget(translation.metadata.modelTarget) ?? modelTarget,
    })).filter((translation): translation is MessageTranslation => translation.modelTarget !== null),
  );
  const threadTranslations: ThreadTranslation[] = session.translations.map((translation) => ({
    locale: translation.locale,
    originalTitle: translation.originalTitle,
    translatedTitle: translation.translatedTitle,
    sourceSha256: translation.sourceSha256,
    translatedMessageIndices: translation.translatedMessageIds
      .map((id) => session.messages.findIndex((message) => message.id === id))
      .filter((index) => index >= 0),
    createdAt: translation.createdAtMs,
    modelTarget: restoreModelTarget(translation.metadata.modelTarget) ?? modelTarget,
  })).filter((translation): translation is ThreadTranslation => translation.modelTarget !== null);
  return {
    ...metadata,
    id: session.id,
    title: session.title,
    messages,
    createdAt: session.createdAtMs,
    updatedAt: session.updatedAtMs,
    pinned: session.pinned,
    unread: metadata.unread === true,
    archived: session.archived,
    groupId: typeof metadata.groupId === "string" ? metadata.groupId : null,
    modelTarget,
    comparisonBranch: metadata.comparisonBranch ?? null,
    crewRun: metadata.crewRun ?? null,
    workspacePath: session.workspacePath,
    personaId: session.personaId,
    attachedStackIds: Array.isArray(metadata.attachedStackIds) ? metadata.attachedStackIds : [],
    docChatMode: metadata.docChatMode === true,
    subagentRuns: metadata.subagentRuns && typeof metadata.subagentRuns === "object" ? metadata.subagentRuns : {},
    subagentRunMeta: metadata.subagentRunMeta && typeof metadata.subagentRunMeta === "object" ? metadata.subagentRunMeta : {},
    messageTranslations,
    threadTranslations,
    displayTranslationLocale: typeof metadata.displayTranslationLocale === "string" ? metadata.displayTranslationLocale : null,
  } as ChatSession;
}

function portableRestoreSettings(metadata: Record<string, unknown>): PortableRestoreSettings | null {
  const raw = metadata.settings;
  if (!raw || typeof raw !== "object") return null;
  const rawLocale = (raw as { locale?: unknown }).locale;
  const locale = typeof rawLocale === "string" && LOCALES.some((entry) => entry.code === rawLocale)
    ? rawLocale
    : null;
  const rawOverrides = (raw as { shortcutOverrides?: unknown }).shortcutOverrides;
  let shortcutOverrides: PortableShortcutOverrides | null = null;
  if (rawOverrides && typeof rawOverrides === "object") {
    const payload = JSON.stringify({ version: SHORTCUT_STORAGE_VERSION, overrides: rawOverrides });
    shortcutOverrides = hydrateShortcutOverrides(payload);
  }
  return locale || shortcutOverrides ? { locale, shortcutOverrides } : null;
}

function currentSessionPayload(): string {
  const state = useSessionStore.getState();
  return JSON.stringify({
    sessions: state.sessions,
    activeSessionId: state.activeSessionId,
    groups: state.groups,
    crews: state.crews,
  });
}

/** Writes browser-storage mirrors as one compensated client step. Rust keeps
 * the same settings in a pending file until this succeeds, so a crash or a
 * disabled/full localStorage cannot silently lose the restored preference. */
function applyPortableSettings(settings: PortableRestoreSettings): boolean {
  let oldLocale: string | null = null;
  let oldShortcuts: string | null = null;
  let persisted = true;
  try {
    oldLocale = localStorage.getItem(LOCALE_STORAGE_KEY);
    oldShortcuts = localStorage.getItem(SHORTCUT_STORAGE_KEY);
    if (settings.locale) localStorage.setItem(LOCALE_STORAGE_KEY, settings.locale);
    if (settings.shortcutOverrides) {
      localStorage.setItem(SHORTCUT_STORAGE_KEY, JSON.stringify({
        version: SHORTCUT_STORAGE_VERSION,
        overrides: settings.shortcutOverrides,
      }));
    }
  } catch {
    persisted = false;
    try {
      if (oldLocale === null) localStorage.removeItem(LOCALE_STORAGE_KEY);
      else localStorage.setItem(LOCALE_STORAGE_KEY, oldLocale);
      if (oldShortcuts === null) localStorage.removeItem(SHORTCUT_STORAGE_KEY);
      else localStorage.setItem(SHORTCUT_STORAGE_KEY, oldShortcuts);
    } catch {
      // Rust's pending settings file remains the durable recovery source.
    }
  }
  if (settings.locale) useLocaleStore.setState({ locale: settings.locale as LocaleCode });
  if (settings.shortcutOverrides) {
    useShortcutStore.setState({ overrides: settings.shortcutOverrides });
  }
  return persisted;
}

async function acknowledgePortableSettings(transactionId: string): Promise<void> {
  await invoke("portable_restore_settings_acknowledge", { transactionId });
}

/** Replays settings left pending by a crash between the Rust file commit and
 * the browser-storage mirror. This module is part of the eagerly imported UI
 * bundle, so recovery starts on every desktop launch without a settings-panel
 * visit. */
export async function recoverPendingPortableSettings(): Promise<boolean> {
  const pending = await invoke<PendingPortableRestoreSettings | null>("portable_restore_settings_pending");
  if (!pending) return false;
  const locale = typeof pending.locale === "string" && LOCALES.some((entry) => entry.code === pending.locale)
    ? pending.locale
    : null;
  let shortcutOverrides: PortableShortcutOverrides | null = null;
  if (pending.shortcutOverrides && typeof pending.shortcutOverrides === "object") {
    const payload = JSON.stringify({
      version: SHORTCUT_STORAGE_VERSION,
      overrides: pending.shortcutOverrides,
    });
    shortcutOverrides = hydrateShortcutOverrides(payload);
  }
  const settings = { locale, shortcutOverrides };
  if (applyPortableSettings(settings)) await acknowledgePortableSettings(pending.transactionId);
  return true;
}

export async function importPortableOutcome(outcome: PortableReadOutcome, mode: "merge" | "replace"): Promise<number> {
  const artifacts = new Map(outcome.artifacts.map((artifact) => [artifact.id, artifact]));
  const sessions = outcome.data.sessions
    .slice()
    .sort((left, right) => left.ordinal - right.ordinal)
    .map((session) => reconstructedSession(session, artifacts));
  const groups = Array.isArray(outcome.data.metadata.groups)
    ? outcome.data.metadata.groups as SessionGroup[]
    : [];
  const crews = Array.isArray(outcome.data.metadata.crews)
    ? outcome.data.metadata.crews as CrewDefinition[]
    : [];
  const stackDefinitions = Array.isArray(outcome.data.metadata.stackDefinitions)
    ? outcome.data.metadata.stackDefinitions as KnowledgeStack[]
    : [];
  const sessionPlan = planPortableSessionImport(useSessionStore.getState(), sessions, mode, { groups, crews });
  const prompts = outcome.data.metadata.prompts;
  let promptEntries: PromptEntry[] = [];
  let defaultPersonaId: string | null = null;
  if (prompts && typeof prompts === "object" && Array.isArray((prompts as { entries?: unknown }).entries)) {
    promptEntries = (prompts as { entries: unknown[] }).entries.filter((entry): entry is PromptEntry => {
      if (!entry || typeof entry !== "object") return false;
      const candidate = entry as Partial<PromptEntry>;
      return typeof candidate.id === "string" &&
        (candidate.kind === "persona" || candidate.kind === "snippet") &&
        typeof candidate.name === "string" &&
        typeof candidate.command === "string" &&
        typeof candidate.content === "string" &&
        typeof candidate.createdAt === "number" &&
        typeof candidate.updatedAt === "number";
    });
    defaultPersonaId = typeof (prompts as { defaultPersonaId?: unknown }).defaultPersonaId === "string"
      ? (prompts as { defaultPersonaId: string }).defaultPersonaId
      : null;
  }
  const promptPlan = planPortablePromptImport(
    usePromptStore.getState(),
    promptEntries,
    defaultPersonaId,
    mode,
  );
  const settings = portableRestoreSettings(outcome.data.metadata);
  const command = await invoke<PortableRestoreCommandOutcome>("portable_restore_apply", {
    request: {
      mode,
      sessionsPayload: portableSessionPlanPayload(sessionPlan),
      previousSessionsPayload: currentSessionPayload(),
      promptsPayload: portablePromptPlanPayload(promptPlan),
      stacks: stackDefinitions,
      settings,
    },
  });

  // No in-memory mutation occurs before the single backend transaction
  // succeeds. These assignments cannot schedule a second persistence pass.
  applyPortableSessionImportPlan(sessionPlan);
  applyPortablePromptImportPlan(promptPlan);
  useStackStore.setState({ stacks: command.stacks });
  if (settings && applyPortableSettings(settings) && command.settingsPending) {
    await acknowledgePortableSettings(command.transactionId);
  }
  return sessionPlan.imported;
}

export async function exportPortableBundle(path: string, sessionIds?: readonly string[]): Promise<PortablePreflightReport> {
  return invoke("portable_export_bundle", { path, request: await buildPortableBundleRequest(sessionIds) });
}

export async function exportPortableSession(path: string, session: ChatSession, format: "markdown" | "json" | "docx"): Promise<void> {
  await invoke("portable_export_session", { path, format, session: await portableSession(session) });
}

export async function readPortableBundle(path: string): Promise<PortableReadOutcome> {
  return invoke("portable_read_bundle", { path });
}

export async function createEncryptedSnapshot(retention?: SnapshotRetentionPolicy): Promise<SnapshotWriteOutcome> {
  return invoke("portable_snapshot_create", { request: await buildPortableBundleRequest(), retention: retention ?? null });
}

export async function listEncryptedSnapshots(): Promise<SnapshotFileInfo[]> {
  return invoke("portable_snapshot_list");
}

export async function openEncryptedSnapshot(path: string): Promise<PortableReadOutcome> {
  return invoke("portable_snapshot_open", { path });
}

export async function getWebDavBackupStatus(): Promise<WebDavBackupStatus> {
  return invoke("portable_webdav_status_get");
}

export async function stageEncryptedSnapshot(): Promise<WebDavStagedSnapshot> {
  return invoke("portable_snapshot_stage_source", { request: await buildPortableBundleRequest() });
}

export async function runWebDavBackupDue(force = false): Promise<WebDavBackgroundRunOutcome> {
  return invoke("portable_webdav_run_due", { force });
}

export async function saveWebDavConfig(request: {
  enabled: boolean;
  baseUrl: string;
  username: string;
  password: string | null;
  remotePath: string;
  intervalMinutes: number;
}): Promise<WebDavBackupConfig> {
  return invoke("portable_webdav_config_save", { request });
}

export async function testWebDav(): Promise<void> {
  await invoke("portable_webdav_test");
}

export async function downloadSnapshotFromWebDav(): Promise<
  | { status: "downloaded"; remotePath: string; etag: string; payload: PortableReadOutcome }
  | { status: "not_modified" | "missing" }
> {
  return invoke("portable_webdav_download_snapshot");
}

if (isTauri()) {
  void recoverPendingPortableSettings().catch((error) => {
    console.error("Failed to recover pending portable settings", error);
  });
}
