/**
 * Checkpoint Preview and State-Aware Rollback (ROADMAP.md Phase 1) — the
 * "external state" classifier.
 *
 * The existing checkpoint system (`src-tauri/src/checkpoints.rs`) tracks a
 * single coarse `shellRan` flag per checkpoint: whether `tool_run_shell` ran
 * during the turn, which file restore can never undo. That's a reliable
 * signal (it's written by the backend itself, and survives even if the
 * transcript's tool-call messages are later dropped by context compaction —
 * see `contextTrimmer.ts`) but it's also the ONLY external-effect signal the
 * backend can see. The transcript, while it's still around, has much finer
 * detail: exactly which tools ran, so a network call (`web_fetch`/
 * `web_search`) or an MCP tool invocation (arbitrary external side effects —
 * a database write, an API call, a message send) can be named specifically
 * instead of lumped under "a shell command ran".
 *
 * This module is that finer-grained classifier: a pure function of the
 * turn's own `ChatMessage[]` slice (same "derive from the persisted
 * transcript" idiom `extractArtifacts`/the `[Checkpoint]`/`[Verify]` notices
 * already use — see `agentLoop.ts`'s doc comment), never touching the
 * filesystem or invoking anything. Its output plus the backend's `shellRan`
 * together answer the acceptance criterion: "Unsupported external effects
 * are marked `needs_reconciliation` instead of being silently retried or
 * reversed" — see `needsReconciliation` below, which is deliberately an OR
 * of both signals so a compacted-away shell call still gets flagged even
 * when the transcript detail is gone.
 */
import type { ChatMessage } from './llamaClient';

/** Tools that mutate workspace files — already captured exactly by the
 * checkpoint's own manifest entries (see `checkpoints.rs`'s `record_original`
 * and the new `checkpoint_preview`/`checkpoint_compare` commands), so a call
 * to one of these is "file state", not "external state", even though it's a
 * real mutation. Mirrors `turnEngine.ts`'s `RISK_ELIGIBLE_TOOLS` minus
 * `run_shell` (which belongs in `EXTERNAL_TOOL_KINDS` below instead). */
export const FILE_TOOL_NAMES = new Set(['write_file', 'edit_file']);

/** What kind of non-file side effect an external-effect tool has — shown in
 * the UI so "a shell command ran" and "a network call happened" read as
 * distinct, specific warnings rather than one generic caveat. */
export type ExternalEffectKind = 'shell' | 'network' | 'memory' | 'mcp' | 'task-suggestion';

/** Plain (non-MCP) tool names with a real side effect outside the
 * checkpointed workspace, and the kind each one is:
 * - `run_shell`: arbitrary shell/process side effects (the manifest's
 *   `shellRan` already tracks this coarsely; this adds the specific name).
 * - `web_fetch`/`web_search`: network calls — an HTTP request already
 *   happened and can't be un-sent.
 * - `remember`: writes to this app's own persistent memory store, a form of
 *   state that lives outside the checkpointed workspace files.
 * - `spawn_task`: stages a follow-up chip. Nothing runs until the user clicks
 *   it, but the chip outlives the turn — a reverted turn that keeps proposing
 *   work is proposing it on the strength of something the user took back. */
const EXTERNAL_TOOL_KINDS: Record<string, ExternalEffectKind> = {
  run_shell: 'shell',
  web_fetch: 'network',
  web_search: 'network',
  remember: 'memory',
  spawn_task: 'task-suggestion',
};

/** MCP tool calls (`mcp__<server>__<tool>`) are always external: an MCP
 * server can do literally anything (write to a database, call a third-party
 * API, send a message) and this app has no way to know or undo it. Matches
 * `turnEngine.ts::isPermissionGatedTool`'s own `mcp__` prefix check. */
export function isMcpToolName(name: string): boolean {
  return name.startsWith('mcp__');
}

/** Classifies one tool-call name into an [`ExternalEffectKind`], or `null`
 * if it isn't an external-effect tool at all (a file tool, a pure-read tool
 * like `read_file`/`grep`, `present_plan`, etc.). */
export function classifyExternalTool(name: string): ExternalEffectKind | null {
  if (isMcpToolName(name)) return 'mcp';
  return EXTERNAL_TOOL_KINDS[name] ?? null;
}

/** One distinct external-effect tool that ran during a turn. */
export interface ExternalEffect {
  tool: string;
  kind: ExternalEffectKind;
}

/** Everything this classifier found in one turn's message slice. */
export interface TurnToolEffects {
  /** Distinct file-mutating tool names invoked (informational — the
   * checkpoint's own file entries are still the source of truth for WHICH
   * files changed). */
  fileTools: string[];
  /** Distinct external-effect tools invoked, newest-call-order deduplicated
   * to first occurrence. */
  external: ExternalEffect[];
}

const EMPTY_TURN_TOOL_EFFECTS: TurnToolEffects = { fileTools: [], external: [] };

/**
 * The `[start, end)` message-index range covering the turn anchored at
 * `anchorIndex`: from the turn's own user message up to (but not including)
 * the next `user`-role message in the transcript, or the end of the array if
 * this is the most recent turn. Every assistant/tool message produced while
 * that turn ran falls inside this range — the same boundary
 * `checkpointAnchorValid` implicitly relies on `anchorIndex` pointing at.
 * Returns `[anchorIndex, anchorIndex]` (empty) for an out-of-range index.
 */
export function turnMessageRange(messages: ChatMessage[], anchorIndex: number): [number, number] {
  if (!Number.isInteger(anchorIndex) || anchorIndex < 0 || anchorIndex >= messages.length) {
    return [anchorIndex, anchorIndex];
  }
  for (let i = anchorIndex + 1; i < messages.length; i++) {
    if (messages[i].role === 'user') return [anchorIndex, i];
  }
  return [anchorIndex, messages.length];
}

/**
 * Scans every `assistant` message's `tool_calls` within the turn anchored at
 * `anchorIndex` and buckets each distinct tool name into "file" or
 * "external". Pure and synchronous — no store reads, no `invoke`.
 */
export function classifyTurnToolCalls(messages: ChatMessage[], anchorIndex: number): TurnToolEffects {
  const [start, end] = turnMessageRange(messages, anchorIndex);
  if (start >= end) return EMPTY_TURN_TOOL_EFFECTS;

  const fileTools = new Set<string>();
  const external = new Map<string, ExternalEffectKind>();

  for (let i = start; i < end; i++) {
    const message = messages[i];
    if (message.role !== 'assistant' || !message.tool_calls) continue;
    for (const call of message.tool_calls) {
      const name = call.function.name;
      if (FILE_TOOL_NAMES.has(name)) {
        fileTools.add(name);
        continue;
      }
      const kind = classifyExternalTool(name);
      if (kind && !external.has(name)) external.set(name, kind);
    }
  }

  return {
    fileTools: [...fileTools],
    external: [...external.entries()].map(([tool, kind]) => ({ tool, kind })),
  };
}

/**
 * Whether a checkpoint's rollback needs manual reconciliation beyond a plain
 * file restore — the acceptance criterion's `needs_reconciliation` gate.
 * `true` whenever EITHER signal says an external effect happened:
 * - `shellRan`: the backend-tracked flag (`CheckpointManifest.shell_ran` /
 *   `CheckpointInfo.shellRan`) — reliable even if the transcript's tool-call
 *   messages were later compacted away.
 * - `external`: this module's finer-grained, transcript-derived list (see
 *   `classifyTurnToolCalls`) — richer when the transcript is still intact,
 *   but silently empty (never falsely "safe") once compaction drops the
 *   detail, which is exactly why it's OR'd with `shellRan` rather than
 *   replacing it.
 *
 * Deliberately a plain boolean OR, never a guess at whether a specific
 * effect is "probably fine to ignore" — per the roadmap, an external effect
 * this system can't deterministically undo must always surface as
 * `needs_reconciliation`, not be silently skipped or auto-reversed.
 */
export function needsReconciliation(shellRan: boolean, external: ExternalEffect[]): boolean {
  return shellRan || external.length > 0;
}
