# Little Monkey — Shell SDK

`curl`-based examples for Little Monkey's local Private Developer API
(Settings > API Server). Copy the function(s) you need from `client.sh`.

## Setup

```sh
export LMK_BASE_URL="http://127.0.0.1:1234/v1"   # from Settings > API Server
export LMK_TOKEN="lmk-..."                        # shown once when you create the token
source ./client.sh
lmk_chat "qwen2.5-7b-instruct" "Hello!"
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
be routed to, independent of scope. There is no route to submit a new
workflow run over this API — only to read an existing run's status.

## Functions

- `lmk_health` — unauthenticated liveness probe.
- `lmk_models` — the merged model listing visible to this token.
- `lmk_chat MODEL MESSAGE` / `lmk_chat_stream MODEL MESSAGE` — chat
  completions, buffered or streamed (`-N`).
- `lmk_knowledge_query STACK_ID QUERY` — hybrid search over an
  already-indexed knowledge stack.
- `lmk_workflow_run_status RUN_ID` — read-only run status.
- `lmk_artifact_read ARTIFACT_ID` — base64-encoded artifact bytes.
