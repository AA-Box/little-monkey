#!/usr/bin/env bash
# Little Monkey Private Developer API -- shell/curl examples.
#
# Not meant to be `source`d blindly into a production script as-is -- these
# are documented examples. Copy the function(s) you need. See ./README.md
# for scopes and the auth header format.
#
# Configure via environment variables:
#   LMK_BASE_URL   e.g. http://127.0.0.1:1234/v1  (from Settings > API Server)
#   LMK_TOKEN      the plaintext token minted in Settings > API Server

set -euo pipefail

: "${LMK_BASE_URL:?Set LMK_BASE_URL, e.g. http://127.0.0.1:1234/v1}"
: "${LMK_TOKEN:?Set LMK_TOKEN to a token minted in Settings > API Server}"

lmk_health() {
  # /health lives at the server root, not under /v1.
  local root="${LMK_BASE_URL%/v1}"
  curl -sS "${root}/health"
}

lmk_models() {
  curl -sS "${LMK_BASE_URL}/models" \
    -H "Authorization: Bearer ${LMK_TOKEN}"
}

lmk_chat() {
  local model="$1" message="$2"
  curl -sS "${LMK_BASE_URL}/chat/completions" \
    -H "Authorization: Bearer ${LMK_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "$(printf '{"model":"%s","messages":[{"role":"user","content":"%s"}],"stream":false}' "$model" "$message")"
}

lmk_chat_stream() {
  # -N disables curl's output buffering so SSE chunks print as they arrive.
  local model="$1" message="$2"
  curl -sS -N "${LMK_BASE_URL}/chat/completions" \
    -H "Authorization: Bearer ${LMK_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "$(printf '{"model":"%s","messages":[{"role":"user","content":"%s"}],"stream":true}' "$model" "$message")"
}

lmk_knowledge_query() {
  local stack_id="$1" query="$2"
  curl -sS "${LMK_BASE_URL}/knowledge/query" \
    -H "Authorization: Bearer ${LMK_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "$(printf '{"stack_id":"%s","query":"%s"}' "$stack_id" "$query")"
}

lmk_workflow_run_status() {
  local run_id="$1"
  curl -sS "${LMK_BASE_URL}/workflows/runs/${run_id}" \
    -H "Authorization: Bearer ${LMK_TOKEN}"
}

lmk_artifact_read() {
  local artifact_id="$1"
  curl -sS "${LMK_BASE_URL}/artifacts/${artifact_id}" \
    -H "Authorization: Bearer ${LMK_TOKEN}"
}

# Example (uncomment to run directly: ./client.sh):
# lmk_health
# lmk_models
# lmk_chat "qwen2.5-7b-instruct" "Hello!"
