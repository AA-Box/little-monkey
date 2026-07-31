/**
 * Streaming chat client for configured cloud AI providers (OpenAI,
 * Anthropic, Google Gemini, OpenRouter, or a custom OpenAI-compatible
 * endpoint) — see `src-tauri/src/providers.rs`.
 *
 * Unlike `llamaClient.ts`'s `streamChat` (a plain `fetch` against an
 * unauthenticated local endpoint), this never touches the network directly:
 * the API key is a billable secret that lives in the OS keychain and is
 * only ever read by Rust, for the lifetime of a single request. This module
 * just invokes the Rust-side proxy and bridges its `provider://chat-*`
 * events back into the same pull-based `AsyncGenerator<StreamEvent>` shape
 * `streamChat` produces, reusing its `SseEventParser` for the wire-format
 * details so both transports interpret OpenAI's SSE shape identically.
 */
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { SseEventParser } from './llamaClient';
import type { ChatMessage, StreamEvent, ToolDef } from './llamaClient';
import { errorMessage } from "./errors";

interface ChatChunkEvent {
  request_id: string;
  chunk: string;
}
interface ChatErrorEvent {
  request_id: string;
  message: string;
}
interface ChatDoneEvent {
  request_id: string;
  /** Set by the Rust side when this stream ended via `providers_cancel_chat`
   * (Stop button) rather than the model finishing on its own — tells the
   * listener below to skip `parser.flush()`, which would otherwise
   * synthesize a bogus tool call from a partial in-flight fragment. */
  cancelled?: boolean;
}

/** Internal queue entry — a real StreamEvent, or a terminal error to throw once drained. */
type QueueEntry = StreamEvent | { type: 'error'; message: string };

/**
 * Streams a chat completion from `providerId` (must already have a saved
 * key — see `modelStore.ts`'s `setProviderKey`). Throws if the underlying
 * request fails at any point (missing key, network error, non-2xx
 * response) — same failure contract as `streamChat`.
 */
export async function* streamProviderChat(
  providerId: string,
  model: string,
  messages: ChatMessage[],
  tools: ToolDef[],
  signal?: AbortSignal,
  effort?: string,
  runId?: string,
): AsyncGenerator<StreamEvent> {
  if (signal?.aborted) return;

  const requestId = crypto.randomUUID();
  const parser = new SseEventParser();

  // Bridges the callback-based `listen()` events into a pull-based
  // generator: each event pushes onto `queue` and wakes up the `for await`
  // loop below (via `waiter`) instead of it having to poll.
  const queue: QueueEntry[] = [];
  let waiter: (() => void) | null = null;
  let finished = false;

  function wake() {
    if (!waiter) return;
    const resolve = waiter;
    waiter = null;
    resolve();
  }

  function pushEvents(events: Iterable<StreamEvent>) {
    for (const event of events) queue.push(event);
    wake();
  }

  function fail(message: string) {
    if (finished) return; // a terminal event already arrived — don't pile on.
    queue.push({ type: 'error', message });
    finished = true;
    wake();
  }

  const unlistenChunk = await listen<ChatChunkEvent>('provider://chat-chunk', (event) => {
    if (event.payload.request_id !== requestId) return;
    pushEvents(parser.feed(event.payload.chunk));
  });
  const unlistenError = await listen<ChatErrorEvent>('provider://chat-error', (event) => {
    if (event.payload.request_id !== requestId) return;
    fail(event.payload.message);
  });
  const unlistenDone = await listen<ChatDoneEvent>('provider://chat-done', (event) => {
    if (event.payload.request_id !== requestId) return;
    if (!event.payload.cancelled) pushEvents(parser.flush());
    finished = true;
    wake();
  });

  const onAbort = () => {
    void invoke('providers_cancel_chat', { requestId }).catch(() => {
      // Best-effort — if the stream already finished, Rust just no-ops.
    });
  };
  signal?.addEventListener('abort', onAbort);

  // Fire the proxy command. Its own rejection is only a fallback for a
  // failure that happened before Rust ever got to emit a `chat-error` event
  // (there shouldn't be one, but `fail()` no-ops if a terminal event already
  // landed, so this can never double-report).
  const invocation = invoke<void>('providers_stream_chat', {
    requestId,
    providerId,
    model,
    messages,
    tools,
    effort: effort ?? null,
    runId: runId ?? null,
  }).catch((err: unknown) => {
    fail(errorMessage(err));
  });

  try {
    while (true) {
      while (queue.length > 0) {
        const event = queue.shift();
        if (!event) continue;
        if (event.type === 'error') {
          throw new Error(event.message);
        }
        yield event;
      }
      if (finished) break;
      await new Promise<void>((resolve) => {
        waiter = resolve;
      });
    }
  } finally {
    signal?.removeEventListener('abort', onAbort);
    await unlistenChunk();
    await unlistenError();
    await unlistenDone();
    await invocation;
  }

  yield { type: 'done' };
}
