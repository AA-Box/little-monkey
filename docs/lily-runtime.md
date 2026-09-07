# Lily accelerated backend

Little Monkey can use Perplexity's [Lily](https://github.com/perplexityai/pplx-garden/tree/main/lily) through a loopback-only compatibility bridge. Lily is not a replacement for the managed MLX runtime: it is an aggressively specialized Metal engine for one checkpoint and one hardware generation.

## Supported shape

The bridge deliberately inherits Lily's narrow contract instead of pretending it is a generic provider:

- model: `Qwen3.6-35B-A3B`
- weights: the MLX affine 4-bit/group-64 checkpoint Lily validates
- host: Lily's supported Apple GPU/macOS combination (currently M5-family or newer and macOS 26+)
- input: text-only `system`, `user`, and `assistant` messages
- decoding: Lily's greedy decoding
- tools: unsupported and refused
- vision: unsupported and refused
- Lily upstream: loopback only

If any of those conditions do not hold, keep using Little Monkey's managed MLX, llama.cpp, Ollama, or another provider.

## Why a bridge exists

Little Monkey's generic OpenAI-compatible provider transport consumes SSE streaming. Lily intentionally rejects `stream: true` and returns one blocking chat-completion response. `packaging/lily/lily_bridge.py` adapts that transport mismatch only:

```text
Little Monkey provider request (SSE expected)
            ↓
127.0.0.1 Lily bridge
            ↓  stream=false, text only, no tools
127.0.0.1 Lily
            ↓
blocking completion + usage
            ↓
bridge emits OpenAI chat-completion SSE
```

The bridge preserves Lily's `usage.prompt_tokens_details.cached_tokens` in the synthesized usage chunk, so a Lily prefix-cache hit is not lost at the provider boundary.

## Run

Start Lily using its own documented command and exact supported checkpoint. Verify it answers on its default loopback endpoint:

```sh
curl http://127.0.0.1:8000/health
curl http://127.0.0.1:8000/v1/models
```

Then start the bridge:

```sh
python3 packaging/lily/lily_bridge.py --listen-port 18000
```

The bridge validates Lily's health and requires it to expose exactly `Qwen3.6-35B-A3B` before it binds its own port.

In **Settings → AI Providers**, add a custom OpenAI-compatible provider with:

```text
Base URL: http://127.0.0.1:18000/v1
API key:  local
Model:    Qwen3.6-35B-A3B
```

The current custom-provider form requires a non-empty key. `local` is only a UI sentinel; the bridge never forwards `Authorization` to Lily and Lily receives no credential.

## Prompt cache

Lily itself owns its two-entry decode-state cache. A caller may send `prompt_cache_key`; the key only prefers a matching entry and token-prefix equality remains authoritative. The bridge passes that field through when present and preserves Lily's reported cached-token count in the OpenAI usage event.

Little Monkey's managed text MLX service has a separate two-entry strict-prefix cache. It reuses a decode state only when the cached token sequence is a strict prefix of the newly rendered prompt. A cache key may prefer an entry but cannot bypass token equality. Vision requests are deliberately excluded until the VLM cache can prove equivalent token/image state.

## Verification

The bridge tests use a fake Lily process and require no Apple GPU or model:

```sh
python3 packaging/lily/test_lily_bridge.py
```

The managed MLX protocol/cache tests likewise stub MLX:

```sh
python3 packaging/mlx/service/test_mlx_server.py
```

A release package build additionally runs `pip check` and verifies the exact installed MLX package versions before signing. Real-model performance and model-architecture compatibility remain opt-in hardware checks; they are not inferred from these protocol tests.
