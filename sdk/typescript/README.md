# Little Monkey — TypeScript SDK

A single-file, dependency-free `fetch` client for Little Monkey's local
Private Developer API (Settings > API Server). Copy `client.ts` into your
own project — it isn't published to any registry.

## Setup

1. In Little Monkey, open **Settings > API Server** and turn the server on.
2. Create a token with the scopes your app needs (see below).
3. Copy the base URL shown in the panel, e.g. `http://127.0.0.1:1234/v1`.

```ts
import { LittleMonkeyClient } from "./client";

const client = new LittleMonkeyClient({
  baseUrl: "http://127.0.0.1:1234/v1",
  token: "lmk-...", // shown once when you create the token — store it yourself
});

const reply = await client.chat({
  model: "qwen2.5-7b-instruct",
  messages: [{ role: "user", content: "Hello!" }],
});
```

## Auth header format

Every request except `GET /health` and the `OPTIONS` CORS preflight must
carry:

```
Authorization: Bearer <token>
```

An expired or revoked token gets the same `401` as a token that never
existed — the server never reveals which case it was.

## Scopes

| Scope           | Gates                                              |
| --------------- | --------------------------------------------------- |
| `chat`           | `POST /v1/chat/completions`                         |
| `models`         | `GET /v1/models`                                    |
| `embeddings`     | `POST /v1/embeddings`                               |
| `knowledge`      | `POST /v1/knowledge/query`                          |
| `workflow_run`   | `GET /v1/workflows/runs/{id}` (status only — see below) |
| `artifact_read`  | `GET /v1/artifacts/{id}`                            |

A token also carries a `backends` list (`local` / `ollama` / `providers`)
that further restricts which upstream a `chat`/`models`/`embeddings` call may
be routed to, independent of scope.

There is deliberately no method or route to *submit* a new workflow run over
this API — `workflow_run` only ever gates a read-only status lookup. Wiring
run submission needs its own per-run approval design (mirroring
`src-tauri/src/permissions.rs`) before it can be added safely; see the doc
comment on `handle_extended_request` in `src-tauri/src/server.rs`.

## Methods

- `chat(request)` / `chatStream(request)` — chat completions, non-streaming
  or as an async generator of SSE deltas.
- `models()` — the merged model listing visible to this token.
- `knowledgeQuery(request)` — hybrid search over an already-indexed
  knowledge stack.
- `workflowRunStatus(runId)` — read-only run status.
- `artifactRead(id)` — base64-encoded artifact bytes.
- `health()` — unauthenticated liveness probe.
