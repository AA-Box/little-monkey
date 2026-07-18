/**
 * Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item 14)
 * — cloud provider side.
 *
 * Mirrors `visionModels.ts`'s shape: a thin, synchronous read of already-
 * fetched store state, not a fresh Rust round trip per lookup. The actual
 * check against the retired-model registry runs once per provider model-list
 * refresh (see `modelStore.ts`'s `setProviderKey`/`refreshProviderModels`,
 * which call the `providers_check_model_retirements` command) and is cached
 * in `providerModelRetirements`, so rendering a long model list (OpenRouter
 * alone can return 400+) never needs hundreds of individual lookups.
 *
 * The retirement registry itself lives in Rust
 * (`src-tauri/src/model_retirement.rs`) — see that module's doc comment for
 * the honest maintenance story: a conservative, versioned local list, not a
 * live-verified source, since there is no upstream API this app can call in
 * this sandbox to ask "is this model retired?".
 */
import { useModelStore, type CloudModelRetirementWarning } from '../store/modelStore';

export type { CloudModelRetirementWarning };

/**
 * Whether `modelId` (from `providerId`) is a known-retired/deprecated cloud
 * model, and if so, its migration hint. Returns `null` both when the model
 * isn't flagged and when this provider's models haven't been checked yet
 * (e.g. immediately after adding a key, before the first check resolves) —
 * callers should treat both cases identically: no warning to show yet.
 */
export function cloudModelRetirementWarning(
  providerId: string,
  modelId: string,
): CloudModelRetirementWarning | null {
  return useModelStore.getState().providerModelRetirements[providerId]?.[modelId] ?? null;
}
