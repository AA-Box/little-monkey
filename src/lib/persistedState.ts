/**
 * The versioned localStorage envelope every workbench store hand-rolled.
 *
 * Nine stores each carried their own `persist()`/`hydrate()` pair with the
 * same shape: write `{ version, ...payload }`, swallow quota and
 * disabled-storage errors, and on read discard anything whose version does
 * not match. The copies drifted in small ways that only show up when
 * something goes wrong — some caught parse errors, some did not; some
 * validated the envelope before trusting it, some assumed it.
 *
 * The on-disk shape here is deliberately the flat `{ version, ...payload }`
 * those stores already write, not a nested `{ version, data }`: a tidier
 * envelope would have silently discarded every existing user's saved panels
 * on first launch after the refactor.
 *
 * Reading is strict and total. A payload that is absent, unparseable,
 * non-object, or written by a different schema version resolves to the
 * caller's fallback rather than throwing — persisted UI state is a
 * convenience, never a source of truth, so a corrupt entry must degrade to
 * "start fresh" instead of breaking the panel that reads it.
 */

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

/**
 * Writes `payload`'s fields alongside a `version` tag. Never throws:
 * localStorage can be full, disabled, or unavailable, and none of those are
 * reasons to fail the user's actual action.
 */
export function persistState(
  key: string,
  version: number,
  payload: Record<string, unknown>,
): void {
  try {
    localStorage.setItem(key, JSON.stringify({ version, ...payload }));
  } catch {
    // Best effort by design — see the module doc.
  }
}

/**
 * Reads `key` and returns the stored envelope only when it parses, is an
 * object, and carries exactly `version`; otherwise `null`.
 *
 * Callers narrow the individual fields themselves — each store already owns
 * per-field validators for its own record types, and those checks are the
 * ones that matter for a payload written by a different build of the same
 * schema version.
 */
export function hydrateState(
  key: string,
  version: number,
): Record<string, unknown> | null {
  try {
    const raw = localStorage.getItem(key);
    if (!raw) return null;
    const parsed: unknown = JSON.parse(raw);
    if (!isRecord(parsed) || parsed.version !== version) return null;
    return parsed;
  } catch {
    return null;
  }
}

/** Removes a persisted entry, ignoring storage errors. */
export function clearPersistedState(key: string): void {
  try {
    localStorage.removeItem(key);
  } catch {
    // Best effort by design — see the module doc.
  }
}
