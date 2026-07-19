/**
 * Little Monkey Private Developer API — TypeScript client.
 *
 * A thin `fetch`-based wrapper around the local API server started from
 * Settings > API Server. It talks only to the base URL you give it (by
 * default your own machine's loopback address) and never anywhere else.
 *
 * See ./README.md for scopes and the auth header format.
 */

export type Scope =
  | "chat"
  | "models"
  | "embeddings"
  | "knowledge"
  | "workflow_run"
  | "artifact_read";

export interface LittleMonkeyClientConfig {
  /** e.g. "http://127.0.0.1:1234/v1" — copy this from Settings > API Server. */
  baseUrl: string;
  /** The plaintext token minted in Settings > API Server. Sent once per
   * request as `Authorization: Bearer <token>`; never logged or persisted
   * by this client. */
  token?: string;
  /** Aborts a request after this many milliseconds. Defaults to 30000. */
  timeoutMs?: number;
}

export interface ChatMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string;
}

export interface ChatCompletionsRequest {
  model: string;
  messages: ChatMessage[];
  stream?: boolean;
  [key: string]: unknown;
}

export interface KnowledgeQueryRequest {
  stack_id: string;
  query: string;
  query_id?: string;
  excluded_source_ids?: string[];
  rerank?: boolean;
  token_budget?: number;
}

export class LittleMonkeyApiError extends Error {
  constructor(
    message: string,
    public status: number,
    public body: unknown,
  ) {
    super(message);
    this.name = "LittleMonkeyApiError";
  }
}

/** Fetch-based client for the local Little Monkey API server. Every method
 * issues exactly one request; none retries or caches. */
export class LittleMonkeyClient {
  private baseUrl: string;
  private token?: string;
  private timeoutMs: number;

  constructor(config: LittleMonkeyClientConfig) {
    this.baseUrl = config.baseUrl.replace(/\/+$/, "");
    this.token = config.token;
    this.timeoutMs = config.timeoutMs ?? 30_000;
  }

  private headers(extra?: Record<string, string>): Record<string, string> {
    const headers: Record<string, string> = { ...extra };
    if (this.token) {
      headers.Authorization = `Bearer ${this.token}`;
    }
    return headers;
  }

  private async request<T>(
    method: string,
    path: string,
    body?: unknown,
    rootRelative = false,
  ): Promise<T> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    const origin = rootRelative ? this.baseUrl.replace(/\/v1$/, "") : this.baseUrl;
    try {
      const response = await fetch(`${origin}${path}`, {
        method,
        headers: this.headers(body === undefined ? {} : { "Content-Type": "application/json" }),
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: controller.signal,
      });
      const text = await response.text();
      const parsed = text.length > 0 ? safeJsonParse(text) : undefined;
      if (!response.ok) {
        throw new LittleMonkeyApiError(
          `${method} ${path} failed with ${response.status}`,
          response.status,
          parsed,
        );
      }
      return parsed as T;
    } finally {
      clearTimeout(timer);
    }
  }

  /** `GET /health` — unauthenticated liveness probe. Note this route lives
   * at the server root, not under `/v1` — the client strips that suffix off
   * `baseUrl` for this one call only. */
  health(): Promise<{ status: string }> {
    return this.request("GET", "/health", undefined, true);
  }

  /** `GET /v1/models` — [`Scope.Models`] not required to be present on every
   * caller; the server itself decides per-token visibility. */
  models(): Promise<{ object: string; data: Array<Record<string, unknown>> }> {
    return this.request("GET", "/models");
  }

  /** `POST /v1/chat/completions` — requires the `chat` scope. Set
   * `request.stream = true` and use {@link chatStream} instead if you want
   * incremental output. */
  chat(request: ChatCompletionsRequest): Promise<Record<string, unknown>> {
    return this.request("POST", "/chat/completions", { ...request, stream: false });
  }

  /** `POST /v1/chat/completions` with `stream: true` — yields each SSE
   * `data:` payload's parsed JSON as it arrives, same framing the OpenAI
   * streaming API uses. Requires the `chat` scope. */
  async *chatStream(
    request: ChatCompletionsRequest,
  ): AsyncGenerator<Record<string, unknown>, void, unknown> {
    const response = await fetch(`${this.baseUrl}/chat/completions`, {
      method: "POST",
      headers: this.headers({ "Content-Type": "application/json" }),
      body: JSON.stringify({ ...request, stream: true }),
    });
    if (!response.ok || !response.body) {
      const text = await response.text();
      throw new LittleMonkeyApiError(
        `POST /chat/completions failed with ${response.status}`,
        response.status,
        safeJsonParse(text),
      );
    }
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines.pop() ?? "";
      for (const line of lines) {
        const trimmed = line.trim();
        if (!trimmed.startsWith("data:")) continue;
        const payload = trimmed.slice("data:".length).trim();
        if (payload === "[DONE]") return;
        yield JSON.parse(payload);
      }
    }
  }

  /** `POST /v1/knowledge/query` — requires the `knowledge` scope. */
  knowledgeQuery(request: KnowledgeQueryRequest): Promise<Record<string, unknown>> {
    return this.request("POST", "/knowledge/query", request);
  }

  /** `GET /v1/workflows/runs/{id}` — read-only run status. Requires the
   * `workflow_run` scope. There is deliberately no method to *submit* a new
   * run over this API — see the server-side module doc comment on why. */
  workflowRunStatus(runId: string): Promise<Record<string, unknown> | null> {
    return this.request("GET", `/workflows/runs/${encodeURIComponent(runId)}`);
  }

  /** `GET /v1/artifacts/{id}` — requires the `artifact_read` scope. */
  artifactRead(id: string): Promise<{ blob: { id: string; size: number }; content_base64: string }> {
    return this.request("GET", `/artifacts/${encodeURIComponent(id)}`);
  }
}

function safeJsonParse(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

export default LittleMonkeyClient;
