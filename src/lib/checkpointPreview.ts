/**
 * Checkpoint Preview and State-Aware Rollback (ROADMAP.md Phase 1) —
 * frontend orchestration layer.
 *
 * Combines two sources into one "what did this checkpoint's turn actually
 * do" view, matching the acceptance criterion "Rollback clearly
 * distinguishes file, artifact, conversation, and external state":
 * - FILE state: `checkpoint_preview`/`checkpoint_compare`/
 *   `checkpoint_simulate_restore` (see `src-tauri/src/checkpoints.rs`) —
 *   exact before/after diffs read from the checkpoint's own backups, never
 *   touching the live workspace.
 * - ARTIFACT, screenshot, and verification state, plus the finer-grained
 *   EXTERNAL-effect breakdown: all transcript-derived, the same "pure
 *   function of `ChatMessage[]`" idiom `extractArtifacts`/the `[Checkpoint]`/
 *   `[Verify]` notices already use (see `agentLoop.ts`'s doc comment) —
 *   scoped to this checkpoint's own turn via `turnMessageRange` (see
 *   `checkpointReconciliation.ts`).
 * CONVERSATION state is just `checkpointAnchorValid` from `agentLoop.ts`,
 * reused rather than reimplemented.
 */
import { invoke } from '@tauri-apps/api/core';

import type { ChatMessage } from './llamaClient';
import { checkpointAnchorValid, isVerifyNotice, parseVerifyNotice, type VerifyNotice } from './agentLoop';
import { extractArtifacts, type ArtifactBlock } from './artifacts';
import { classifyTurnToolCalls, needsReconciliation, turnMessageRange, type ExternalEffect } from './checkpointReconciliation';

// ---------------------------------------------------------------------------
// Types mirroring the Rust `#[serde(rename_all = "camelCase")]` payloads in
// src-tauri/src/checkpoints.rs — kept in lockstep with that file by hand,
// same as every other `invoke`-backed type in this codebase (there is no
// codegen step here).
// ---------------------------------------------------------------------------

export type DiffLineKind = 'context' | 'added' | 'removed';

export interface DiffLine {
  kind: DiffLineKind;
  text: string;
}

export interface DiffResult {
  lines: DiffLine[];
  truncated: boolean;
  added: number;
  removed: number;
}

export type SnapshotSource = 'captured' | 'redo' | 'live' | 'unavailable';

export type FileChangeStatus = 'added' | 'modified' | 'deleted' | 'unchanged' | 'unknown';

export interface FilePreviewEntry {
  path: string;
  status: FileChangeStatus;
  beforeBytes: number;
  afterBytes: number;
  afterSource: SnapshotSource;
  binary: boolean;
  diff: DiffResult | null;
}

export interface CheckpointPreview {
  id: string;
  label: string;
  createdAtMs: number;
  sessionId: string;
  anchorIndex: number;
  shellRan: boolean;
  reverted: boolean;
  files: FilePreviewEntry[];
}

export interface CompareFileEntry {
  path: string;
  inA: boolean;
  inB: boolean;
  a: FilePreviewEntry | null;
  b: FilePreviewEntry | null;
  between: DiffResult | null;
}

export interface CheckpointCompareResult {
  a: CheckpointPreview;
  b: CheckpointPreview;
  files: CompareFileEntry[];
}

export type RestoreAction = 'restore' | 'delete' | 'noOp';

export interface RestorePlanEntry {
  path: string;
  action: RestoreAction;
  drifted: boolean;
}

/** One external effect the backend recorded, and what undoes it. */
export interface ExternalEffectRecord {
  kind: 'shell' | 'network' | 'mcp-tool' | 'memory';
  /** A tagged object rather than a bool, which is what made adding the first
   * real undo a compile error at every reader instead of a flag somebody
   * forgets to check.
   *
   * `undo` exists for `memory`: a remembered fact is this app's own record, and
   * reverting the turn forgets exactly the facts that turn added. The other
   * three are still `none`, each with its own reason — a shell command can
   * change anything, a request cannot be un-sent, an MCP server is outside this
   * app. */
  compensation: { kind: 'none'; reason: string } | { kind: 'undo'; action: string };
}

export interface RestoreSimulation {
  id: string;
  alreadyReverted: boolean;
  files: RestorePlanEntry[];
  /** True when any recorded effect has no compensator. Derived from
   * `externalEffects`, not from `shellRan` — a turn that only made a network
   * call used to report `false` here and read as "nothing to reconcile". */
  needsReconciliation: boolean;
  /** Recorded in the manifest when each effect happened, so — unlike
   * `classifyTurnToolCalls`'s transcript-derived list, which is finer-grained
   * but only survives while the messages do — this is still here after a
   * context compaction. */
  externalEffects: ExternalEffectRecord[];
}

/** Fetches checkpoint `id`'s per-file diff/status preview. Read-only. */
export async function fetchCheckpointPreview(id: string): Promise<CheckpointPreview> {
  return invoke<CheckpointPreview>('checkpoint_preview', { id });
}

/** Compares two checkpoints' file state without restoring either. Read-only. */
export async function fetchCheckpointCompare(idA: string, idB: string): Promise<CheckpointCompareResult> {
  return invoke<CheckpointCompareResult>('checkpoint_compare', { idA, idB });
}

/** Simulates reverting checkpoint `id` — what WOULD change — without doing
 * it. Read-only. */
export async function fetchCheckpointSimulateRestore(id: string): Promise<RestoreSimulation> {
  return invoke<RestoreSimulation>('checkpoint_simulate_restore', { id });
}

// ---------------------------------------------------------------------------
// Transcript-derived turn context (artifact/screenshot/verify/external/
// conversation state) — pure functions of ChatMessage[], no invoke.
// ---------------------------------------------------------------------------

/** One image attachment found within a checkpoint's turn — e.g. a browser
 * verification screenshot staged via `BrowserWorkbench.tsx`'s "Attach"
 * button (see `browserWorkbenchStore.ts`'s `BrowserChatEvidence`), which
 * lands as an `image_url` content part on the turn's own user message.
 * Generic rather than browser-specific: any image attachment surfaces here,
 * since the transcript has no separate "this came from the browser" tag. */
export interface CheckpointImageAttachment {
  messageIndex: number;
  url: string;
}

/** Everything transcript-derived about one checkpoint's turn — the
 * artifact/conversation/external three-quarters of the acceptance
 * criterion's four-way state split (file state is `CheckpointPreview`,
 * fetched separately from the backend). */
export interface CheckpointTurnContext {
  /** Artifacts (HTML/SVG/Mermaid fences) produced anywhere within this
   * turn's messages — see `artifacts.ts`'s `extractArtifacts`. */
  artifacts: ArtifactBlock[];
  /** Image attachments within this turn's messages. */
  images: CheckpointImageAttachment[];
  /** Verification command results reported during this turn (see
   * `agentLoop.ts`'s `VERIFY_NOTE_PREFIX`). */
  verify: VerifyNotice[];
  /** Tool-call-derived external effects (network calls, MCP calls, shell —
   * see `checkpointReconciliation.ts`) for this turn. */
  external: ExternalEffect[];
  /** The UI's `needs_reconciliation` gate: true when file restore alone
   * can't safely/deterministically cover this checkpoint's rollback. */
  needsReconciliation: boolean;
  /** Whether "Rewind conversation" is available for this checkpoint (the
   * anchor still resolves to a matching user message) — reuses
   * `agentLoop.ts`'s `checkpointAnchorValid` rather than reimplementing it. */
  conversationRewindAvailable: boolean;
}

/**
 * Builds a checkpoint's [`CheckpointTurnContext`] from the session's full
 * transcript. Pure and synchronous — every field is derived from `messages`
 * alone, scoped to the turn anchored at `anchorIndex` via `turnMessageRange`.
 * `shellRan` is the backend-tracked flag from the checkpoint's own manifest
 * (`CheckpointInfo.shellRan`/`CheckpointPreview.shellRan`), folded into
 * `needsReconciliation` alongside the transcript-derived `external` list.
 */
export function gatherTurnContext(
  messages: ChatMessage[],
  anchorIndex: number,
  label: string,
  shellRan: boolean,
): CheckpointTurnContext {
  const [start, end] = turnMessageRange(messages, anchorIndex);

  const artifacts = extractArtifacts(messages).filter(
    (block) => block.ref.messageIndex >= start && block.ref.messageIndex < end,
  );

  const images: CheckpointImageAttachment[] = [];
  const verify: VerifyNotice[] = [];
  for (let i = start; i < end; i++) {
    const message = messages[i];
    if (Array.isArray(message.content)) {
      for (const part of message.content) {
        if (part.type === 'image_url') images.push({ messageIndex: i, url: part.image_url.url });
      }
    }
    if (isVerifyNotice(message)) {
      const notice = parseVerifyNotice(message);
      if (notice) verify.push(notice);
    }
  }

  const { external } = classifyTurnToolCalls(messages, anchorIndex);
  const conversationRewindAvailable = checkpointAnchorValid(messages, { id: '', files: [], anchorIndex, label });

  return {
    artifacts,
    images,
    verify,
    external,
    needsReconciliation: needsReconciliation(shellRan, external),
    conversationRewindAvailable,
  };
}

/** Full checkpoint preview: the backend's file-state breakdown plus the
 * transcript-derived turn context, fetched/gathered together for the
 * preview panel. */
export interface CheckpointFullPreview extends CheckpointTurnContext {
  filePreview: CheckpointPreview;
}

/**
 * Loads everything `CheckpointPreviewPanel` needs for one checkpoint: the
 * backend's per-file diff/status breakdown, plus artifact/screenshot/verify/
 * external/conversation state derived from `messages`. `messages` should be
 * the live session transcript at call time (`sessionMessages(sessionId)`) —
 * this function itself makes no store reads.
 */
export async function loadCheckpointFullPreview(
  messages: ChatMessage[],
  info: { id: string; anchorIndex: number; label: string; shellRan: boolean },
): Promise<CheckpointFullPreview> {
  const filePreview = await fetchCheckpointPreview(info.id);
  return { filePreview, ...gatherTurnContext(messages, info.anchorIndex, info.label, info.shellRan) };
}
