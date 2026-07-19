"use strict";

const LOOPBACK_HOSTS = new Set(["127.0.0.1", "localhost", "[::1]", "::1"]);
const MAX_CONTEXT_CHARS = 120_000;

function validateCompletionConfiguration(config) {
  if (!config.enabled) return { enabled: false };
  if (!config.model || !config.fimCapableModels.includes(config.model)) {
    throw new Error("Choose a completion model and declare its exact tag FIM-capable first");
  }
  const origin = new URL(config.host);
  if (origin.protocol !== "http:" || !LOOPBACK_HOSTS.has(origin.hostname)) {
    throw new Error("Little Monkey completions only connect to loopback Ollama over HTTP");
  }
  return { enabled: true, origin, model: config.model };
}

function boundedFimContext(text, offset) {
  const half = Math.floor(MAX_CONTEXT_CHARS / 2);
  return {
    prefix: text.slice(Math.max(0, offset - half), offset),
    suffix: text.slice(offset, Math.min(text.length, offset + half)),
  };
}

class OllamaFimService {
  constructor(fetchImpl = globalThis.fetch) {
    this.fetchImpl = fetchImpl;
    this.active = new Map();
    this.insertCapable = new Set();
  }

  cancel(key) {
    this.active.get(key)?.abort();
    this.active.delete(key);
  }

  async complete(request) {
    const validated = validateCompletionConfiguration(request.config);
    if (!validated.enabled) return null;
    this.cancel(request.documentKey);
    const controller = new AbortController();
    this.active.set(request.documentKey, controller);
    const disposeCancellation = request.onCancel?.(() => controller.abort());
    try {
      if (request.debounceMs > 0) {
        await new Promise((resolve, reject) => {
          const timer = setTimeout(resolve, request.debounceMs);
          controller.signal.addEventListener("abort", () => {
            clearTimeout(timer);
            reject(new DOMException("Cancelled", "AbortError"));
          }, { once: true });
        });
      }
      if (request.currentVersion() !== request.version) return null;
      const context = boundedFimContext(request.text, request.offset);
      const capabilityKey = `${validated.origin.origin}\0${validated.model}`;
      if (!this.insertCapable.has(capabilityKey)) {
        const shown = await this.fetchImpl(new URL("/api/show", validated.origin), {
          method: "POST",
          headers: { "content-type": "application/json" },
          signal: controller.signal,
          body: JSON.stringify({ model: validated.model, verbose: false }),
        });
        const details = await shown.json().catch(() => ({}));
        if (!shown.ok) throw new Error(details.error || `Ollama model inspection failed with HTTP ${shown.status}`);
        if (!Array.isArray(details.capabilities) || !details.capabilities.includes("insert")) {
          throw new Error(`Ollama model ${validated.model} does not advertise the required insert capability`);
        }
        this.insertCapable.add(capabilityKey);
      }
      if (request.currentVersion() !== request.version) return null;
      const response = await this.fetchImpl(new URL("/api/generate", validated.origin), {
        method: "POST",
        headers: { "content-type": "application/json" },
        signal: controller.signal,
        body: JSON.stringify({
          model: validated.model,
          // Ollama's native suffix field is the portable FIM boundary. The
          // selected model must still be explicitly declared FIM-capable.
          prompt: context.prefix,
          suffix: context.suffix,
          raw: true,
          stream: false,
          think: false,
          keep_alive: "10m",
          options: {
            num_predict: request.maxTokens,
            temperature: 0.1,
            stop: ["<|fim_prefix|>", "<|fim_suffix|>", "<|fim_middle|>"]
          }
        })
      });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) throw new Error(payload.error || `Ollama completion failed with HTTP ${response.status}`);
      if (request.currentVersion() !== request.version || controller.signal.aborted) return null;
      return typeof payload.response === "string" ? payload.response : null;
    } finally {
      disposeCancellation?.dispose?.();
      if (this.active.get(request.documentKey) === controller) this.active.delete(request.documentKey);
    }
  }

  dispose() {
    for (const controller of this.active.values()) controller.abort();
    this.active.clear();
    this.insertCapable.clear();
  }
}

module.exports = {
  OllamaFimService,
  boundedFimContext,
  validateCompletionConfiguration,
};
