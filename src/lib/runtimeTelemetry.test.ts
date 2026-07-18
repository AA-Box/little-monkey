import { describe, expect, it } from "vitest";

import type { RuntimeTraceRecord, SupportBundle } from "./runtimeHubClient";
import {
  extractSamplerStatsFromRequestBody,
  extractTokenTimingFromResponseBody,
  groupTracesByModel,
  serializeSupportBundle,
  supportBundleFileName,
} from "./runtimeTelemetry";

function bytesOf(text: string): number[] {
  return Array.from(new TextEncoder().encode(text));
}

describe("extractSamplerStatsFromRequestBody", () => {
  it("reads only the known numeric sampler keys", () => {
    const body = JSON.stringify({
      model: "qwen2.5-7b",
      messages: [{ role: "user", content: "what is the capital of France?" }],
      temperature: 0.7,
      top_p: 0.9,
      max_tokens: 256,
      stream: false,
    });
    const stats = extractSamplerStatsFromRequestBody(bytesOf(body));
    expect(stats.temperature).toBe(0.7);
    expect(stats.topP).toBe(0.9);
    expect(stats.maxOutputTokens).toBe(256);
    expect(stats.topK).toBeNull();
    expect(stats.seed).toBeNull();
  });

  it("never surfaces prompt/message content even when present", () => {
    const body = JSON.stringify({
      messages: [{ role: "user", content: "my secret prompt" }],
      temperature: 0.2,
    });
    const stats = extractSamplerStatsFromRequestBody(bytesOf(body));
    const serialized = JSON.stringify(stats);
    expect(serialized).not.toContain("secret prompt");
    expect(Object.keys(stats).sort()).toEqual(
      ["maxOutputTokens", "repeatPenalty", "seed", "temperature", "topK", "topP"].sort(),
    );
  });

  it("falls back to num_predict for Ollama-native max token key", () => {
    const stats = extractSamplerStatsFromRequestBody(bytesOf(JSON.stringify({ num_predict: 128 })));
    expect(stats.maxOutputTokens).toBe(128);
  });

  it("returns an all-null result for malformed JSON instead of throwing", () => {
    expect(() => extractSamplerStatsFromRequestBody(bytesOf("{not json"))).not.toThrow();
    const stats = extractSamplerStatsFromRequestBody(bytesOf("{not json"));
    expect(stats.temperature).toBeNull();
  });

  it("returns an all-null result for a non-object JSON body", () => {
    const stats = extractSamplerStatsFromRequestBody(bytesOf("42"));
    expect(stats.temperature).toBeNull();
    expect(stats.seed).toBeNull();
  });
});

describe("extractTokenTimingFromResponseBody", () => {
  it("reads OpenAI-shaped usage counts", () => {
    const tokens = extractTokenTimingFromResponseBody({
      choices: [{ message: { role: "assistant", content: "the answer is 4" } }],
      usage: { prompt_tokens: 12, completion_tokens: 34 },
    });
    expect(tokens.inputTokens).toBe(12);
    expect(tokens.outputTokens).toBe(34);
  });

  it("never surfaces response content", () => {
    const tokens = extractTokenTimingFromResponseBody({
      choices: [{ message: { content: "a secret answer" } }],
      usage: { prompt_tokens: 1, completion_tokens: 2 },
    });
    expect(JSON.stringify(tokens)).not.toContain("secret answer");
  });

  it("handles a non-object body without throwing", () => {
    expect(extractTokenTimingFromResponseBody(null).inputTokens).toBeNull();
    expect(extractTokenTimingFromResponseBody("plain string").outputTokens).toBeNull();
  });
});

function fixtureTrace(overrides: Partial<RuntimeTraceRecord> = {}): RuntimeTraceRecord {
  return {
    schemaVersion: 1,
    traceId: "t1",
    runtimeId: "llama-cpp",
    modelId: "qwen2.5-7b",
    recordedAtMs: 1_000,
    outcome: "success",
    errorMessage: null,
    event: {
      kind: "load",
      timing: { startedAtMs: 0, readyAtMs: 100, durationMs: 100 },
      offload: null,
      memory: null,
    },
    unavailable: [],
    ...overrides,
  };
}

describe("groupTracesByModel", () => {
  it("groups traces by modelId preserving order", () => {
    const traces = [
      fixtureTrace({ traceId: "a", modelId: "model-1" }),
      fixtureTrace({ traceId: "b", modelId: "model-2" }),
      fixtureTrace({ traceId: "c", modelId: "model-1" }),
    ];
    const groups = groupTracesByModel(traces);
    expect(Array.from(groups.keys())).toEqual(["model-1", "model-2"]);
    expect(groups.get("model-1")?.map((trace) => trace.traceId)).toEqual(["a", "c"]);
    expect(groups.get("model-2")?.map((trace) => trace.traceId)).toEqual(["b"]);
  });

  it("returns an empty map for no traces", () => {
    expect(groupTracesByModel([]).size).toBe(0);
  });
});

function fixtureBundle(overrides: Partial<SupportBundle> = {}): SupportBundle {
  return {
    schemaVersion: 1,
    generatedAtMs: 1_700_000_000_000,
    appVersion: "0.0.0-test",
    platform: "macos",
    hardware: null,
    compatibility: null,
    traces: [],
    runtimeLogs: [],
    redactionTotals: { findingsRedacted: 0, byKind: {} },
    excluded: ["Prompt and response text"],
    ...overrides,
  };
}

describe("supportBundleFileName", () => {
  it("includes the platform and is a stable, filesystem-safe name", () => {
    const name = supportBundleFileName(fixtureBundle(), 1_700_000_000_000);
    expect(name).toMatch(/^little-monkey-support-bundle-macos-.+\.json$/);
    expect(name).not.toContain(":");
  });
});

describe("serializeSupportBundle", () => {
  it("serializes the bundle exactly as given (no re-redaction, no mutation)", () => {
    const bundle = fixtureBundle({
      runtimeLogs: [{ runtimeId: "llama-cpp", text: "context size: 4096", truncated: false, redaction: { findingsRedacted: 0, byKind: {} } }],
    });
    const serialized = serializeSupportBundle(bundle);
    expect(JSON.parse(serialized)).toEqual(bundle);
    expect(serialized).toContain("context size: 4096");
  });
});
