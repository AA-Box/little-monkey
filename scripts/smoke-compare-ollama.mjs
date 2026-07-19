#!/usr/bin/env node

/**
 * Opt-in live acceptance smoke for M1.1's Ollama transport. It deliberately
 * stays outside the default test suite because loading four real models can
 * take minutes and substantial RAM/VRAM.
 *
 * Usage:
 *   pnpm test:compare:live -- model-a model-b model-c model-d
 *
 * With no model arguments, three smallest local tags plus one Ollama cloud
 * tag are selected when available; otherwise four local tags are used.
 * Branches run sequentially to protect memory.
 */

const baseUrl = (process.env.OLLAMA_BASE_URL || "http://127.0.0.1:11434").replace(/\/+$/, "");
const timeoutMs = Number(process.env.COMPARE_SMOKE_TIMEOUT_MS || 10 * 60 * 1000);
const maxTokens = Number(process.env.COMPARE_SMOKE_MAX_TOKENS || 512);
const requestedModels = process.argv.slice(2).filter((value) => value.trim().length > 0);

if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) {
  throw new Error("COMPARE_SMOKE_TIMEOUT_MS must be a positive number");
}
if (!Number.isInteger(maxTokens) || maxTokens < 1 || maxTokens > 4096) {
  throw new Error("COMPARE_SMOKE_MAX_TOKENS must be an integer between 1 and 4096");
}
if (requestedModels.length > 0 && (requestedModels.length < 2 || requestedModels.length > 4)) {
  throw new Error("Pass between two and four distinct Ollama model tags");
}
if (new Set(requestedModels).size !== requestedModels.length) {
  throw new Error("Every live comparison model tag must be unique");
}

async function requestJson(path, init = {}, timeout = 15_000) {
  const signal = AbortSignal.timeout(timeout);
  const response = await fetch(`${baseUrl}${path}`, { ...init, signal });
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`${path} failed (${response.status} ${response.statusText})${text ? `: ${text}` : ""}`);
  }
  return text ? JSON.parse(text) : {};
}

async function runningModels() {
  const payload = await requestJson("/api/ps");
  if (!Array.isArray(payload.models)) return [];
  return payload.models.flatMap((entry) => {
    const name = typeof entry?.name === "string" ? entry.name : typeof entry?.model === "string" ? entry.model : "";
    return name ? [name] : [];
  });
}

async function chooseModels() {
  if (requestedModels.length > 0) return requestedModels;
  const payload = await requestJson("/api/tags");
  const entries = Array.isArray(payload.models) ? payload.models : [];
  const candidates = entries.flatMap((entry) => {
    const name = typeof entry?.name === "string" ? entry.name : typeof entry?.model === "string" ? entry.model : "";
    const size = typeof entry?.size === "number" ? entry.size : 0;
    const cloud = Boolean(entry?.remote_host) || name.includes("-cloud");
    return name ? [{ name, size, cloud }] : [];
  });
  const local = candidates.filter((entry) => !entry.cloud && entry.size > 0).sort((left, right) => left.size - right.size);
  const cloud = candidates.filter((entry) => entry.cloud).sort((left, right) => left.name.localeCompare(right.name));
  if (local.length >= 3 && cloud.length >= 1) {
    return [...local.slice(0, 3), cloud[0]].map((entry) => entry.name);
  }
  if (local.length >= 4) return local.slice(0, 4).map((entry) => entry.name);
  throw new Error(`Need either three local plus one cloud tag, or four local tags; found ${local.length} local and ${cloud.length} cloud`);
}

function parseEventBlock(block) {
  const data = block
    .split(/\r?\n/)
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trimStart())
    .join("\n");
  if (!data || data === "[DONE]") return null;
  return JSON.parse(data);
}

async function unloadIfOwned(model, preexisting) {
  if (preexisting.has(model)) return "preserved-preexisting";
  if (!(await runningModels()).includes(model)) return "already-released";
  await requestJson(
    "/api/chat",
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model, messages: [], keep_alive: 0, stream: false }),
    },
    30_000,
  );
  return "released";
}

async function streamBranch(model, frozenMessages, preexisting) {
  const startedAt = Date.now();
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(new Error(`Timed out after ${timeoutMs} ms`)), timeoutMs);
  let content = "";
  let usage = null;
  let requestedTools = false;
  let cleanup = "not-needed";

  try {
    const response = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      signal: controller.signal,
      body: JSON.stringify({
        model,
        messages: structuredClone(frozenMessages),
        stream: true,
        stream_options: { include_usage: true },
        // Some reasoning-capable Ollama models emit hidden reasoning before
        // their visible content. Keep enough headroom for that prelude while
        // retaining a hard smoke-test bound.
        max_tokens: maxTokens,
        temperature: 0,
      }),
    });
    if (!response.ok || !response.body) {
      const detail = await response.text();
      throw new Error(`chat failed (${response.status} ${response.statusText})${detail ? `: ${detail}` : ""}`);
    }

    const decoder = new TextDecoder();
    let buffer = "";
    for await (const chunk of response.body) {
      buffer += decoder.decode(chunk, { stream: true });
      const blocks = buffer.split(/\r?\n\r?\n/);
      buffer = blocks.pop() || "";
      for (const block of blocks) {
        const event = parseEventBlock(block);
        if (!event) continue;
        const delta = event.choices?.[0]?.delta;
        if (typeof delta?.content === "string") content += delta.content;
        if (Array.isArray(delta?.tool_calls) && delta.tool_calls.length > 0) requestedTools = true;
        if (event.usage && typeof event.usage === "object") usage = event.usage;
      }
    }
    buffer += decoder.decode();
    if (buffer.trim()) {
      const event = parseEventBlock(buffer);
      const delta = event?.choices?.[0]?.delta;
      if (typeof delta?.content === "string") content += delta.content;
      if (Array.isArray(delta?.tool_calls) && delta.tool_calls.length > 0) requestedTools = true;
      if (event?.usage && typeof event.usage === "object") usage = event.usage;
    }

    if (requestedTools) throw new Error("model emitted a tool call in a no-tools comparison");
    if (!content.trim()) throw new Error("model returned no visible response content");
    return { model, ok: true, content: content.trim(), usage, durationMs: Date.now() - startedAt };
  } finally {
    clearTimeout(timeout);
    try {
      cleanup = await unloadIfOwned(model, preexisting);
    } catch (error) {
      cleanup = `cleanup-warning: ${error instanceof Error ? error.message : String(error)}`;
    }
    process.stdout.write(`${model}: ${cleanup}\n`);
  }
}

const models = await chooseModels();
const preexisting = new Set(await runningModels());
const frozenMessages = [
  {
    role: "system",
    content:
      "You are a read-only branch in a model comparison. No tools are available. Answer only the user prompt.",
  },
  { role: "user", content: "Reply with exactly OK and nothing else. /no_think" },
];
const results = [];

process.stdout.write(`Live Compare smoke: ${models.join(" | ")}\n`);
process.stdout.write(`Pre-existing resident models: ${[...preexisting].join(", ") || "none"}\n`);

for (const [index, model] of models.entries()) {
  process.stdout.write(`[${index + 1}/${models.length}] ${model}\n`);
  try {
    const result = await streamBranch(model, frozenMessages, preexisting);
    results.push(result);
    process.stdout.write(`${model}: ${result.durationMs} ms, ${result.usage?.total_tokens ?? "unknown"} tokens\n`);
  } catch (error) {
    results.push({
      model,
      ok: false,
      error: error instanceof Error ? error.message : String(error),
    });
    process.stderr.write(`${model}: FAILED: ${results[results.length - 1].error}\n`);
  }
}

process.stdout.write(`${JSON.stringify({ models, results }, null, 2)}\n`);
if (results.some((result) => !result.ok)) process.exitCode = 1;
