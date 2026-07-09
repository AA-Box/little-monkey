/**
 * Adaptive context compaction: once a conversation's estimated token count
 * crosses a user-configured percentage of the active model's context
 * window (`settingsStore`'s `contextTrimThreshold`), the oldest complete
 * turns are either dropped (`'trim'`) or replaced with a one-shot model
 * summary (`'summarize'`) — see `settingsStore`'s `contextTrimStrategy`.
 *
 * Both strategies insert a visible marker message into the *actual* session
 * transcript (not just the outgoing wire payload), so the user can see that
 * compaction happened instead of history silently vanishing. This means
 * compaction is a one-time transformation that persists, like Claude Code's
 * own `/compact`, rather than being redone from scratch every turn.
 */
import { textContent, type ChatMessage } from './llamaClient';
import type { ContextTrimStrategy } from '../store/settingsStore';

/** Prefix identifying a synthetic compaction marker, so the UI (`MessageBubble.tsx`) can style it distinctly and this module can recognize its own past markers. */
export const COMPACTION_MARKER_PREFIX = '[Context compacted]';

/** Rough chars-per-token ratio for the token estimate below — not an exact
 * tokenizer count (none is available client-side for every provider), just
 * enough to decide "are we probably close to the limit". */
const CHARS_PER_TOKEN_ESTIMATE = 4;
/** Flat per-image token estimate — vision tokenizers vary a lot by provider and image size; this is a conservative placeholder. */
const IMAGE_TOKEN_ESTIMATE = 500;
/** Never cut so close to the end that fewer than this many messages remain — a single very long-running turn shouldn't get its own immediate context torn apart. */
const MIN_RECENT_MESSAGES_KEPT = 4;
/** After compacting, aim to land at this fraction of the configured threshold rather than exactly at it, so the very next turn doesn't immediately re-trigger. */
const TARGET_HEADROOM_FACTOR = 0.7;
/** Cap on how much of any single dropped message's text is inlined into the summarization prompt, so one huge tool result can't blow up the summary request itself. */
const MAX_SUMMARY_SOURCE_CHARS = 2000;

export function isCompactionMarker(message: ChatMessage): boolean {
  return message.role === 'system' && typeof message.content === 'string' && message.content.startsWith(COMPACTION_MARKER_PREFIX);
}

function estimateMessageTokens(message: ChatMessage): number {
  if (typeof message.content === 'string') {
    return Math.ceil(message.content.length / CHARS_PER_TOKEN_ESTIMATE);
  }
  return message.content.reduce((sum, part) => {
    if (part.type === 'text') return sum + Math.ceil(part.text.length / CHARS_PER_TOKEN_ESTIMATE);
    return sum + IMAGE_TOKEN_ESTIMATE;
  }, 0);
}

/** Approximate token count for the whole history — see the module doc's caveat about this being an estimate, not an exact tokenizer count. */
export function estimateHistoryTokens(history: ChatMessage[]): number {
  return history.reduce((sum, message) => sum + estimateMessageTokens(message), 0);
}

/** Whether `history` has crossed `thresholdPercent` of `contextLimit`. `contextLimit` of `null`/`0` (unknown context window) always returns `false` — compaction needs a real budget to aim for. */
export function shouldTrim(history: ChatMessage[], contextLimit: number | null, thresholdPercent: number): boolean {
  if (!contextLimit || contextLimit <= 0) return false;
  return (estimateHistoryTokens(history) / contextLimit) * 100 >= thresholdPercent;
}

function leadingSystemPrefixLength(history: ChatMessage[]): number {
  let i = 0;
  while (i < history.length && history[i].role === 'system') i++;
  return i;
}

/** Indices (>= `from`) of every `user`-role message — each one safely starts a self-contained turn, since a user message can never be the tool-result half of an orphaned tool_call/tool-result pair. */
function userTurnBoundaries(history: ChatMessage[], from: number): number[] {
  const indices: number[] = [];
  for (let i = from; i < history.length; i++) {
    if (history[i].role === 'user') indices.push(i);
  }
  return indices;
}

/** Renders a run of messages about to be dropped into plain text for the summarization prompt — tool calls/results are described rather than replayed verbatim, and any single message's text is capped so one huge result can't dominate the prompt. */
function renderForSummary(messages: ChatMessage[]): string {
  return messages
    .map((message) => {
      if (message.role === 'tool') {
        const result = textContent(message.content).slice(0, MAX_SUMMARY_SOURCE_CHARS);
        return `[tool result] ${result}`;
      }
      const toolCalls = message.tool_calls ?? [];
      const text = textContent(message.content).slice(0, MAX_SUMMARY_SOURCE_CHARS);
      const hasImage = typeof message.content !== 'string' && message.content.some((part) => part.type === 'image_url');
      const parts = [text, hasImage ? '[image attached]' : '', ...toolCalls.map((tc) => `[called tool ${tc.function.name}(${tc.function.arguments})]`)];
      return `${message.role}: ${parts.filter(Boolean).join(' ')}`;
    })
    .join('\n');
}

export interface CompactionResult {
  messages: ChatMessage[];
  changed: boolean;
}

export interface CompactionOptions {
  strategy: ContextTrimStrategy;
  contextLimit: number | null;
  thresholdPercent: number;
  /** Sends the dropped messages to the model for a one-shot summary. Only called for `strategy === 'summarize'`. Any rejection falls back to a plain trim marker rather than losing the compaction. */
  sendForSummary: (dropped: ChatMessage[]) => Promise<string>;
}

/**
 * Picks the oldest safe run of complete turns to drop (or summarize) so the
 * remaining history lands comfortably under the threshold, and returns the
 * new message array with a visible marker in place of what was removed.
 * Returns `{changed: false}` untouched if there aren't at least two user
 * turns after any leading system message(s) — trimming a single ongoing
 * turn would destroy the only context the model has, so it's skipped rather
 * than forced.
 */
export async function applyContextCompaction(history: ChatMessage[], options: CompactionOptions): Promise<CompactionResult> {
  const prefixLen = leadingSystemPrefixLength(history);
  const boundaries = userTurnBoundaries(history, prefixLen);
  if (boundaries.length < 2) return { messages: history, changed: false };

  const targetPercent = options.thresholdPercent * TARGET_HEADROOM_FACTOR;

  let cut = boundaries[0];
  for (const boundaryIndex of boundaries) {
    if (history.length - boundaryIndex < MIN_RECENT_MESSAGES_KEPT) break;
    cut = boundaryIndex;
    if (options.contextLimit) {
      const remainderEstimate = estimateHistoryTokens(history.slice(boundaryIndex));
      if ((remainderEstimate / options.contextLimit) * 100 <= targetPercent) break;
    }
  }

  if (cut <= prefixLen) return { messages: history, changed: false };

  const pinnedPrefix = history.slice(0, prefixLen);
  const dropped = history.slice(prefixLen, cut);
  const kept = history.slice(cut);

  let markerText: string;
  if (options.strategy === 'summarize') {
    try {
      const summary = await options.sendForSummary(dropped);
      markerText = `${COMPACTION_MARKER_PREFIX} Summarized ${dropped.length} earlier messages:\n${summary}`;
    } catch {
      markerText = `${COMPACTION_MARKER_PREFIX} Removed ${dropped.length} earlier messages to fit the context window (summarization failed, dropped instead).`;
    }
  } else {
    markerText = `${COMPACTION_MARKER_PREFIX} Removed ${dropped.length} earlier messages to fit the context window.`;
  }

  const marker: ChatMessage = { role: 'system', content: markerText };
  return { messages: [...pinnedPrefix, marker, ...kept], changed: true };
}

/** Exported for `agentLoop.ts`'s `sendForSummary` implementation, which needs to turn the dropped chunk into the actual summarization prompt text. */
export { renderForSummary };
