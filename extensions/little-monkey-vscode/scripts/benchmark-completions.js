"use strict";

const vm = require("node:vm");
const { performance } = require("node:perf_hooks");
const { OllamaFimService } = require("../src/completion");
const { buildCompletionFixtures } = require("../test/completionFixtures");

async function main() {
  const host = process.env.LITTLE_MONKEY_COMPLETION_HOST || "http://127.0.0.1:11434";
  const model = process.env.LITTLE_MONKEY_COMPLETION_MODEL;
  if (!model) throw new Error("Set LITTLE_MONKEY_COMPLETION_MODEL to an exact local FIM-capable Ollama tag");
  const service = new OllamaFimService();
  const latencies = [];
  const failures = [];
  let compiled = 0;
  try {
    for (const fixture of buildCompletionFixtures()) {
      const text = `${fixture.prefix}${fixture.suffix}`;
      const started = performance.now();
      const completion = await service.complete({
        config: { enabled: true, model, fimCapableModels: [model], host },
        documentKey: fixture.id,
        version: 1,
        currentVersion: () => 1,
        text,
        offset: fixture.prefix.length,
        maxTokens: 96,
        debounceMs: 0,
      });
      latencies.push(performance.now() - started);
      try {
        new vm.Script(`${fixture.prefix}${completion ?? ""}${fixture.suffix}`, { filename: `${fixture.id}.js` });
        compiled += 1;
      } catch (error) {
        failures.push({ id: fixture.id, error: error.message, completion });
      }
    }
  } finally {
    service.dispose();
  }
  latencies.sort((a, b) => a - b);
  const p95Ms = latencies[Math.max(0, Math.ceil(latencies.length * 0.95) - 1)];
  let residency = null;
  try {
    const response = await fetch(new URL("/api/ps", host));
    const payload = await response.json();
    const entry = payload.models?.find((candidate) => candidate.name === model || candidate.model === model);
    if (entry) residency = { sizeBytes: entry.size, sizeVramBytes: entry.size_vram, expiresAt: entry.expires_at };
  } catch (_) {}
  const report = { model, host, fixtures: 100, compiled, p95Ms, residency, failures };
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
  if (compiled < 70 || p95Ms >= 750) process.exitCode = 2;
}

main().catch((error) => {
  process.stderr.write(`${error.stack || error.message}\n`);
  process.exitCode = 1;
});
