/**
 * Best-effort vision-capability classification, used to pick a model when an
 * image is attached (see `agentLoop.ts`'s `buildCandidateTargets`).
 *
 * Ollama models carry a real signal (`OllamaModelInfo.vision`, sourced from
 * Ollama's own `/api/show` `capabilities` array in `ollama.rs`). Cloud
 * provider models don't — `/models` list endpoints only return an `id`
 * (see `ProviderModelInfo` in `modelStore.ts`) — so cloud classification is a
 * best-effort name-pattern heuristic, never presented as a guaranteed fact.
 * Either kind can be corrected via `settingsStore`'s `visionOverrides`, keyed
 * by `providerModelKey`/`ollamaModelKey` below.
 */
import { useSettingsStore } from '../store/settingsStore';
import type { OllamaModelInfo } from '../store/modelStore';

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
  /claude-(opus|sonnet|haiku)-4/,
  /claude-4/,
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

/** Whether `modelId` (from `providerId`) can see images — override if the user has set one, otherwise the name-pattern heuristic above. */
export function isVisionCapableProviderModel(providerId: string, modelId: string): boolean {
  const key = providerModelKey(providerId, modelId);
  const override = useSettingsStore.getState().visionOverrides[key];
  if (override !== undefined) return override;
  return heuristicVisionMatch(modelId);
}

/** Whether an Ollama tag can see images — override if set, otherwise Ollama's own reported capability. */
export function isVisionCapableOllamaModel(model: OllamaModelInfo): boolean {
  const key = ollamaModelKey(model.name);
  const override = useSettingsStore.getState().visionOverrides[key];
  if (override !== undefined) return override;
  return model.vision;
}

/** Local llama.cpp curated/installed models never support vision — `llama.rs` has no `--mmproj`/clip-projector support today. */
export function isVisionCapableLocalModel(): boolean {
  return false;
}
