import type { RuntimeTraceRecord, SamplerStats, SupportBundle, TokenTiming } from "./runtimeHubClient";

/**
 * Reads a fixed allowlist of numeric sampler keys out of a raw request body
 * (the exact bytes sent to `m3_api_dispatch`). Deliberately never reads
 * `messages`/`content`/`input`/`prompt`-shaped keys, so it cannot capture
 * prompt text even though the diagnostics body it is fed can contain any
 * JSON the user typed. Malformed JSON returns an all-`null` result rather
 * than throwing, since sampler capture must never break the request it is
 * observing.
 */
export function extractSamplerStatsFromRequestBody(bytes: number[]): SamplerStats {
  const empty: SamplerStats = {
    temperature: null,
    topP: null,
    topK: null,
    maxOutputTokens: null,
    repeatPenalty: null,
    seed: null,
  };
  let parsed: unknown;
  try {
    const text = new TextDecoder().decode(new Uint8Array(bytes));
    parsed = JSON.parse(text);
  } catch {
    return empty;
  }
  if (typeof parsed !== "object" || parsed === null) return empty;
  const record = parsed as Record<string, unknown>;
  const num = (key: string): number | null => (typeof record[key] === "number" ? (record[key] as number) : null);
  return {
    temperature: num("temperature"),
    topP: num("top_p"),
    topK: num("top_k"),
    maxOutputTokens: num("max_tokens") ?? num("max_output_tokens") ?? num("num_predict"),
    repeatPenalty: num("repeat_penalty"),
    seed: num("seed"),
  };
}

/**
 * Reads the plain `model` identifier out of a raw request body, if present.
 * A model id is a short identifier, not conversational content, so reading
 * it (unlike `messages`/`content`/`input`) is safe.
 */
export function extractModelIdFromRequestBody(bytes: number[]): string | null {
  try {
    const text = new TextDecoder().decode(new Uint8Array(bytes));
    const parsed = JSON.parse(text) as unknown;
    if (typeof parsed !== "object" || parsed === null) return null;
    const model = (parsed as Record<string, unknown>).model;
    return typeof model === "string" && model.length > 0 ? model : null;
  } catch {
    return null;
  }
}

/**
 * Reads a fixed allowlist of numeric usage keys out of a response body.
 * Never reads `choices`/`message`/`content`-shaped keys.
 */
export function extractTokenTimingFromResponseBody(body: unknown): TokenTiming {
  const empty: TokenTiming = {
    inputTokens: null,
    outputTokens: null,
    tokensPerSecond: null,
    cachedPromptTokens: null,
  };
  if (typeof body !== "object" || body === null) return empty;
  const record = body as Record<string, unknown>;
  const num = (value: unknown): number | null => (typeof value === "number" ? value : null);
  const usage =
    typeof record.usage === "object" && record.usage !== null ? (record.usage as Record<string, unknown>) : {};
  return {
    inputTokens: num(usage.prompt_tokens) ?? num(usage.input_tokens),
    outputTokens: num(usage.completion_tokens) ?? num(usage.output_tokens),
    tokensPerSecond: null,
    cachedPromptTokens: null,
  };
}

/** Groups traces by `modelId`, preserving each group's existing order. */
export function groupTracesByModel(traces: RuntimeTraceRecord[]): Map<string, RuntimeTraceRecord[]> {
  const groups = new Map<string, RuntimeTraceRecord[]>();
  for (const trace of traces) {
    const existing = groups.get(trace.modelId);
    if (existing) {
      existing.push(trace);
    } else {
      groups.set(trace.modelId, [trace]);
    }
  }
  return groups;
}

export function supportBundleFileName(bundle: SupportBundle, exportedAtMs = Date.now()): string {
  const stamp = new Date(exportedAtMs).toISOString().replace(/[:.]/g, "-");
  return `little-monkey-support-bundle-${bundle.platform}-${stamp}.json`;
}

/** The bundle returned by `m3_telemetry_support_bundle` is already redacted
 * before it reaches the frontend — this only serializes it for export, it
 * does not add or remove any redaction of its own. */
export function serializeSupportBundle(bundle: SupportBundle): string {
  return JSON.stringify(bundle, null, 2);
}
