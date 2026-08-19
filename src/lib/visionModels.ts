/**
 * Best-effort vision-capability classification, used to pick a model when an
 * image is attached (see `agentLoop.ts`'s `buildCandidateTargets`).
 *
 * Ollama models carry a real signal (`OllamaModelInfo.vision`, sourced from
 * Ollama's own `/api/show` `capabilities` array in `ollama.rs`). Some cloud
 * providers do too — OpenRouter's `architecture.input_modalities` and
 * Anthropic's `capabilities.image_input`, both parsed into
 * `ProviderModelInfo.vision` by `providers.rs` — and that answer wins whenever
 * it exists. The rest (OpenAI, Gemini's OpenAI-compatible shim, custom base
 * URLs) return an id and nothing else, so those fall back to the name-pattern
 * heuristic below, which is a guess and never presented as a guaranteed fact.
 * Any of it can be corrected via `settingsStore`'s `visionOverrides`, keyed
 * by `providerModelKey`/`ollamaModelKey` below.
 */
import { useSettingsStore } from '../store/settingsStore';
import { useModelStore, type OllamaModelInfo } from '../store/modelStore';

/** Known vision-capable model-name fragments, checked against the lowercased id. Not exhaustive — new model names ship constantly. */
const VISION_PATTERNS: RegExp[] = [
  /gpt-4o/,
  /gpt-4\.1/,
  /gpt-4-turbo/,
  /gpt-5/,
  /chatgpt-4o/,
  /^o[134](-|$)/,
  // Covers claude-3-*, claude-3-5-*, claude-3-7-* — the `-5`/`-7` sits between the 3 and the name.
  /claude-3(-\d)?-(opus|sonnet|haiku)/,
  // Name-first ids, any generation: claude-sonnet-4, claude-haiku-4-5, claude-opus-5, claude-fable-5.
  /claude-(opus|sonnet|haiku|fable|mythos)-\d/,
  /claude-[4-9]\b/,
  /gemini/,
  /pixtral/,
  /llava/,
  /moondream/,
  /minicpm-v/,
  /vision/,
  /-vl(-|$)/,
  /qwen.*vl/,
  /llama-3\.2-(11b|90b)-vision/,
];

/** Known text-only exceptions that would otherwise false-positive against `VISION_PATTERNS` (e.g. legacy Claude models). */
const NON_VISION_EXCEPTIONS: RegExp[] = [/claude-instant/, /claude-2\b/, /gpt-3\.5/];

function heuristicVisionMatch(modelId: string): boolean {
  const lower = modelId.toLowerCase();
  if (NON_VISION_EXCEPTIONS.some((pattern) => pattern.test(lower))) return false;
  return VISION_PATTERNS.some((pattern) => pattern.test(lower));
}

/** Override-map key for a cloud provider's model. */
export function providerModelKey(providerId: string, modelId: string): string {
  return `provider:${providerId}:${modelId}`;
}

/** Override-map key for an Ollama tag. */
export function ollamaModelKey(tag: string): string {
  return `ollama:${tag}`;
}

/** Whether `modelId` (from `providerId`) can see images — the user's override if
 * they set one, then what the provider itself reported, and only then the
 * name-pattern heuristic above. */
export function isVisionCapableProviderModel(providerId: string, modelId: string): boolean {
  const key = providerModelKey(providerId, modelId);
  const override = useSettingsStore.getState().visionOverrides[key];
  if (override !== undefined) return override;
  const reported = useModelStore
    .getState()
    .providerModels[providerId]?.find((model) => model.id === modelId)?.vision;
  if (reported !== undefined) return reported;
  return heuristicVisionMatch(modelId);
}

/** Whether an Ollama tag can see images — override if set, otherwise Ollama's own reported capability. */
export function isVisionCapableOllamaModel(model: OllamaModelInfo): boolean {
  const key = ollamaModelKey(model.name);
  const override = useSettingsStore.getState().visionOverrides[key];
  if (override !== undefined) return override;
  return model.vision;
}

/** Local vision is true only for the active bundle after llama-server reports
 * ready with the projector actually loaded. A configured component that has
 * not started successfully is not capability evidence. */
export function isVisionCapableLocalModel(): boolean {
  const state = useModelStore.getState();
  return state.activeProvider === 'local' && state.llamaStatus === 'ready' && state.llamaVisionEnabled;
}
