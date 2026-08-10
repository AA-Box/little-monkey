/**
 * OpenAI-compatible streaming chat client for the local llama-server process.
 *
 * llama.cpp's `llama-server`, when started with `--jinja`, exposes an
 * OpenAI-compatible `/v1/chat/completions` endpoint that supports both plain
 * chat and tool calling. This module talks to that endpoint directly over
 * HTTP (no Tauri involved) and streams the response as Server-Sent Events.
 */

/** A single tool call requested by the model. */
export interface ToolCall {
  id: string;
  type: 'function';
  function: {
    name: string;
    arguments: string;
  };
}

/** One part of a multi-part message `content` array, OpenAI-style. Only ever
 * produced for a `user` message that has an image attachment — see
 * `agentLoop.ts`'s `buildWireContent`. */
export type ChatContentPart =
  | { type: 'text'; text: string }
  | { type: 'image_url'; image_url: { url: string } };

/** A single message in the chat history, OpenAI-style. `content` is a plain
 * string for every text-only message (the overwhelming majority); it's a
 * `ChatContentPart[]` only for a user turn that attached at least one image,
 * so existing text-only code paths never need to branch on the shape. */
export interface ChatMessage {
  role: 'system' | 'user' | 'assistant' | 'tool';
  content: string | ChatContentPart[];
  tool_call_id?: string;
  tool_calls?: ToolCall[];
  /** Local-only: wall-clock time the message entered the transcript, stamped
   * once by `sessionStore.addMessage`. Shown under an assistant answer (see
   * `MessageActions.tsx`) and stripped before every request by
   * {@link toWireMessages}. */
  at?: number;
  /** Local-only: title of the chapter this message was pinned as, or absent
   * when it isn't pinned. Kept on the message rather than in a per-session
   * index map so it survives forks, edits, and truncation without any
   * index bookkeeping; stripped from the wire like `at`. */
  chapter?: string;
}

/** Strips the local-only fields above so a request body carries nothing but
 * the OpenAI-compatible message shape. Both wire paths run this: cloud
 * providers reject unknown message properties outright, and the Rust proxy
 * forwards `messages` as opaque JSON, so it can't do the filtering for us. */
export function toWireMessages(messages: ChatMessage[]): ChatMessage[] {
  return messages.map(({ at: _at, chapter: _chapter, ...wire }) => wire);
}

/** Extracts the plain-text portion of a message's `content` — a no-op for
 * the string case (every `system`/`assistant`/`tool` message, and most
 * `user` messages), or the joined `text` parts for an image-bearing `user`
 * message's `ChatContentPart[]`. Used anywhere a plain string is required
 * (titles, previews, tool results) regardless of which shape `content` is. */
export function textContent(content: ChatMessage['content']): string {
  if (typeof content === 'string') return content;
  return content
    .filter((part): part is { type: 'text'; text: string } => part.type === 'text')
    .map((part) => part.text)
    .join('\n');
}

/** JSON-schema tool definition passed to the model, OpenAI-style. */
export interface ToolDef {
  type: 'function';
  function: {
    name: string;
    description: string;
    parameters: object;
  };
}

/** Events yielded by {@link streamChat} as the response streams in. */
export type StreamEvent =
  | { type: 'delta'; content: string }
  | { type: 'tool_call'; toolCall: ToolCall }
  | { type: 'usage'; usage: { prompt_tokens: number; completion_tokens: number; total_tokens: number } }
  | { type: 'done' };

/** In-progress accumulation state for a single streamed tool call. */
interface PendingToolCall {
  id: string;
  name: string;
  arguments: string;
}

/**
 * Incremental parser for an OpenAI-compatible chat-completions SSE stream.
 * Owns line-buffering (a chunk boundary can land mid-line) and streamed
 * tool-call accumulation (fragments arrive keyed by index across many
 * chunks, only complete once `finish_reason` shows up), so any transport —
 * a `fetch` response body (see `streamChat` below) or Tauri events proxied
 * from the Rust-side provider client (see `providerClient.ts`) — can drive
 * it with nothing more than "here's some more text".
 */
export class SseEventParser {
  private lineBuffer = '';
  private pending = new Map<number, PendingToolCall>();

  /** Feed newly-arrived (already-decoded) text; yields any complete events it produces. */
  *feed(text: string): Generator<Exclude<StreamEvent, { type: 'done' }>> {
    this.lineBuffer += text;
    const lines = this.lineBuffer.split('\n');
    // The last entry may be an incomplete line — keep it in the buffer.
    this.lineBuffer = lines.pop() ?? '';

    for (const line of lines) {
      yield* this.handleLine(line);
    }
  }

  /**
   * Call once the underlying stream has ended: processes any trailing
   * partial line (some servers omit the final newline) and flushes any
   * tool call that was still accumulating when the stream closed.
   */
  *flush(): Generator<Exclude<StreamEvent, { type: 'done' }>> {
    if (this.lineBuffer.trim()) {
      yield* this.handleLine(this.lineBuffer);
      this.lineBuffer = '';
    }
    if (this.pending.size > 0) {
      yield* this.flushPending();
    }
  }

  private *flushPending(): Generator<{ type: 'tool_call'; toolCall: ToolCall }> {
    const indices = Array.from(this.pending.keys()).sort((a, b) => a - b);
    for (const index of indices) {
      const call = this.pending.get(index);
      if (!call) continue;
      yield {
        type: 'tool_call',
        toolCall: {
          id: call.id || `call_${index}`,
          type: 'function',
          function: { name: call.name, arguments: call.arguments },
        },
      };
    }
    this.pending.clear();
  }

  private *handleLine(rawLine: string): Generator<Exclude<StreamEvent, { type: 'done' }>> {
    const line = rawLine.trim();
    if (!line.startsWith('data:')) return;

    const data = line.slice('data:'.length).trim();
    if (!data || data === '[DONE]') return;

    let payload: {
      choices?: Array<{
        delta?: {
          content?: string;
          tool_calls?: Array<{
            index?: number;
            id?: string;
            function?: { name?: string; arguments?: string };
          }>;
        };
        finish_reason?: string | null;
      }>;
      // The final chunk of a stream started with `stream_options:
      // { include_usage: true }` carries this as a SIBLING of `choices`,
      // not nested inside any choice's delta.
      usage?: {
        prompt_tokens: number;
        completion_tokens: number;
        total_tokens: number;
      };
    };

    try {
      payload = JSON.parse(data);
    } catch {
      // Malformed/partial SSE chunk — skip it rather than crashing the loop.
      return;
    }

    if (payload.usage) {
      yield { type: 'usage', usage: payload.usage };
    }

    const choice = payload.choices?.[0];
    if (!choice) return;

    const delta = choice.delta ?? {};

    if (typeof delta.content === 'string' && delta.content.length > 0) {
      yield { type: 'delta', content: delta.content };
    }

    if (Array.isArray(delta.tool_calls)) {
      for (const fragment of delta.tool_calls) {
        const index = typeof fragment.index === 'number' ? fragment.index : 0;
        const existing = this.pending.get(index) ?? { id: '', name: '', arguments: '' };
        if (fragment.id) existing.id = fragment.id;
        if (fragment.function?.name) existing.name = fragment.function.name;
        if (typeof fragment.function?.arguments === 'string') {
          existing.arguments += fragment.function.arguments;
        }
        this.pending.set(index, existing);
      }
    }

    if (choice.finish_reason && this.pending.size > 0) {
      yield* this.flushPending();
    }
  }
}

/**
 * Streams a chat completion from an OpenAI-compatible endpoint (llama-server,
 * or Ollama's own OpenAI-compatible `/v1/chat/completions`). Cloud providers
 * (OpenAI/Anthropic/Gemini/OpenRouter/custom) use `providerClient.ts`'s
 * `streamProviderChat` instead — same `StreamEvent` shape, but proxied
 * through Rust so the API key never enters this WebView.
 *
 * POSTs to `${baseUrl}/v1/chat/completions` with `{ messages, stream: true,
 * stream_options: { include_usage: true }, model }` (plus `tools` and
 * `tool_choice: 'auto'` only when tools are actually offered) and parses the
 * response with {@link SseEventParser}, yielding a
 * final `{ type: 'done' }` once the stream ends. `stream_options.include_usage`
 * is a standard OpenAI-compatible param supported by both llama-server and
 * Ollama; a server that doesn't recognize it simply ignores it.
 *
 * `model` selects which pulled/cloud tag Ollama should use; llama-server
 * ignores the field entirely since it only ever serves the one model it was
 * started with, so the harmless placeholder `"local"` is sent when `model`
 * is omitted.
 */
export async function* streamChat(
  baseUrl: string,
  messages: ChatMessage[],
  tools: ToolDef[],
  model?: string,
  signal?: AbortSignal,
  maxTokens?: number,
): AsyncGenerator<StreamEvent> {
  const url = `${baseUrl.replace(/\/+$/, '')}/v1/chat/completions`;

  let response: Response;
  try {
    const body: Record<string, unknown> = {
      messages: toWireMessages(messages),
      stream: true,
      stream_options: { include_usage: true },
      model: model ?? 'local',
    };
    // Crew runs pass a code-enforced per-call ceiling. OpenAI-compatible
    // llama.cpp/Ollama endpoints honor `max_tokens`; ordinary chat callers
    // omit it and retain their existing behavior.
    if (typeof maxTokens === 'number' && Number.isFinite(maxTokens) && maxTokens > 0) {
      body.max_tokens = Math.floor(maxTokens);
    }
    // Some OpenAI-compatible endpoints reject `tool_choice` when the tools
    // array is empty. Compare deliberately has no tools, so omit both fields
    // entirely instead of sending a contradictory "auto" choice.
    if (tools.length > 0) {
      body.tools = tools;
      body.tool_choice = 'auto';
    }
    response = await fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
      signal,
    });
  } catch (err) {
    // Stop button fired before the request even completed — end the
    // generator quietly instead of surfacing an AbortError.
    if (signal?.aborted) return;
    throw err;
  }

  if (!response.ok || !response.body) {
    let detail = '';
    try {
      detail = await response.text();
    } catch {
      // ignore — body may already be consumed or unreadable
    }
    throw new Error(
      `llama-server request failed (${response.status} ${response.statusText})${
        detail ? `: ${detail}` : ''
      }`
    );
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder('utf-8');
  const parser = new SseEventParser();

  try {
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        yield* parser.feed(decoder.decode(value, { stream: true }));
      }

      const tail = decoder.decode();
      if (tail) yield* parser.feed(tail);
      yield* parser.flush();
    } catch (err) {
      // Stop button aborted the fetch mid-stream — keep whatever content
      // already streamed in and end gracefully rather than throwing (a
      // still-accumulating tool call is deliberately left unflushed, same
      // as the cancel path in providerClient.ts).
      if (!signal?.aborted) throw err;
    }
  } finally {
    reader.releaseLock();
  }

  yield { type: 'done' };
}
