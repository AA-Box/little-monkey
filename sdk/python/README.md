# Little Monkey — Python SDK

A single-file, standard-library-only (`urllib`) client for Little Monkey's
local Private Developer API (Settings > API Server). Copy `client.py` into
your own project — it isn't published to PyPI.

## Setup

1. In Little Monkey, open **Settings > API Server** and turn the server on.
2. Create a token with the scopes your app needs (see below).
3. Copy the base URL shown in the panel, e.g. `http://127.0.0.1:1234/v1`.

```python
from client import LittleMonkeyClient

client = LittleMonkeyClient(base_url="http://127.0.0.1:1234/v1", token="lmk-...")

reply = client.chat(
    model="qwen2.5-7b-instruct",
    messages=[{"role": "user", "content": "Hello!"}],
)
```

## Auth header format

Every request except `GET /health` and the CORS preflight must carry:

```
Authorization: Bearer <token>
```

An expired or revoked token gets the same `401` as a token that never
existed.

## Scopes

| Scope           | Gates                                                    |
| --------------- | --------------------------------------------------------- |
| `chat`           | `POST /v1/chat/completions`                               |
| `models`         | `GET /v1/models`                                          |
| `embeddings`     | `POST /v1/embeddings`                                     |
| `knowledge`      | `POST /v1/knowledge/query`                                |
| `workflow_run`   | `GET /v1/workflows/runs/{id}` (status only)                |
| `artifact_read`  | `GET /v1/artifacts/{id}`                                  |

A token also carries a `backends` list (`local` / `ollama` / `providers`)
that further restricts which upstream a `chat`/`models`/`embeddings` call may
be routed to, independent of scope. There is no method to submit a new
workflow run over this API — only to read an existing run's status.

## Methods

- `chat(model, messages, **extra)` / `chat_stream(...)` — chat completions,
  blocking or as a generator of SSE deltas.
- `models()` — the merged model listing visible to this token.
- `knowledge_query(stack_id, query, ...)` — hybrid search over an
  already-indexed knowledge stack.
- `workflow_run_status(run_id)` — read-only run status.
- `artifact_read(artifact_id)` — base64-encoded artifact bytes.
- `health()` — unauthenticated liveness probe.
