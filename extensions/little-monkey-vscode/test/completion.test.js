"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const vm = require("node:vm");
const {
  OllamaFimService,
  boundedFimContext,
  validateCompletionConfiguration,
} = require("../src/completion");
const { buildCompletionFixtures } = require("./completionFixtures");

const localConfig = {
  enabled: true,
  model: "qwen2.5-coder:7b-fim",
  fimCapableModels: ["qwen2.5-coder:7b-fim"],
  host: "http://127.0.0.1:11434",
};

test("rejects undeclared models and non-loopback targets", () => {
  assert.throws(() => validateCompletionConfiguration({ ...localConfig, fimCapableModels: [] }), /declare/);
  assert.throws(() => validateCompletionConfiguration({ ...localConfig, host: "https://api.example.test" }), /loopback/);
});

test("builds bounded FIM context", () => {
  const text = "a".repeat(200_000);
  const result = boundedFimContext(text, 100_000);
  assert.ok(result.prefix.length <= 60_000);
  assert.ok(result.suffix.length <= 60_000);
});

test("never returns a completion to a newer document version", async () => {
  let version = 1;
  const service = new OllamaFimService(async (url) => {
    if (url.pathname === "/api/show") return { ok: true, json: async () => ({ capabilities: ["completion", "insert"] }) };
    version = 2;
    return { ok: true, json: async () => ({ response: "stale" }) };
  });
  const result = await service.complete({
    config: localConfig,
    documentKey: "file:///workspace/a.ts",
    version: 1,
    currentVersion: () => version,
    text: "const x = ",
    offset: 10,
    maxTokens: 32,
    debounceMs: 0,
  });
  assert.equal(result, null);
});

test("uses Ollama's native suffix boundary and reports no implicit cloud route", async () => {
  let submitted;
  const service = new OllamaFimService(async (url, options) => {
    if (url.pathname === "/api/show") return { ok: true, json: async () => ({ capabilities: ["completion", "insert"] }) };
    submitted = { url: url.toString(), body: JSON.parse(options.body) };
    return { ok: true, json: async () => ({ response: "42" }) };
  });
  const result = await service.complete({
    config: localConfig,
    documentKey: "file:///workspace/native.js",
    version: 1,
    currentVersion: () => 1,
    text: "const answer = ;",
    offset: "const answer = ".length,
    maxTokens: 8,
    debounceMs: 0,
  });
  assert.equal(result, "42");
  assert.equal(submitted.url, "http://127.0.0.1:11434/api/generate");
  assert.equal(submitted.body.prompt, "const answer = ");
  assert.equal(submitted.body.suffix, ";");
  assert.equal(submitted.body.model, localConfig.model);
  assert.equal(submitted.body.stream, false);
});

test("rejects a declared model when Ollama does not advertise insert capability", async () => {
  let generateCalled = false;
  const service = new OllamaFimService(async (url) => {
    if (url.pathname === "/api/show") {
      return { ok: true, json: async () => ({ capabilities: ["completion", "tools"] }) };
    }
    generateCalled = true;
    return { ok: true, json: async () => ({ response: "unsafe" }) };
  });
  await assert.rejects(service.complete({
    config: localConfig,
    documentKey: "capability",
    version: 1,
    currentVersion: () => 1,
    text: "const x = ;",
    offset: 10,
    maxTokens: 8,
    debounceMs: 0,
  }), /insert capability/);
  assert.equal(generateCalled, false);
});

test("at least 70 of 100 maintained completions compile after exact insertion", () => {
  const fixtures = buildCompletionFixtures();
  assert.equal(fixtures.length, 100);
  const ids = new Set();
  let compiled = 0;
  for (const fixture of fixtures) {
    assert.ok(!ids.has(fixture.id), `duplicate fixture ${fixture.id}`);
    ids.add(fixture.id);
    try {
      new vm.Script(`${fixture.prefix}${fixture.completion}${fixture.suffix}`, {
        filename: `${fixture.id}.js`,
      });
      compiled += 1;
    } catch (_) {
      // Stable fixture ids make individual failures actionable while the
      // aggregate threshold remains the release gate.
    }
  }
  assert.ok(compiled >= 70, `${compiled}/100 completion fixtures compiled`);
});

test("cancels the previous request for the same document", async () => {
  const service = new OllamaFimService((_url, options) => new Promise((_resolve, reject) => {
    options.signal.addEventListener("abort", () => reject(new DOMException("Cancelled", "AbortError")), { once: true });
  }));
  const first = service.complete({
    config: localConfig,
    documentKey: "same",
    version: 1,
    currentVersion: () => 1,
    text: "a",
    offset: 1,
    maxTokens: 8,
    debounceMs: 0,
  });
  const second = service.complete({
    config: localConfig,
    documentKey: "same",
    version: 1,
    currentVersion: () => 1,
    text: "b",
    offset: 1,
    maxTokens: 8,
    debounceMs: 10_000,
  });
  await assert.rejects(first, /Cancel/);
  service.dispose();
  await assert.rejects(second, /Cancel/);
});
