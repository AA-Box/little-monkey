import { create } from "zustand";

import {
  SHORTCUTS,
  defaultShortcutBindings,
  detectShortcutPlatform,
  effectiveShortcutBindings,
  findShortcutConflict,
  sanitizeShortcutBinding,
  shortcutBindingsConflict,
  shortcutBindingsEqual,
  shortcutById,
  validateShortcutBinding,
  type ShortcutBinding,
  type ShortcutId,
  type ShortcutOverrides,
  type ShortcutPlatformInput,
  type ShortcutValidationError,
} from "../lib/shortcuts";

/** Versioned separately from the legacy all-purpose settings blob. */
export const SHORTCUT_STORAGE_KEY = "little-monkey-shortcuts";
export const SHORTCUT_STORAGE_VERSION = 1 as const;
export const MAX_SHORTCUT_BINDINGS = 4;

interface PersistedShortcutsV1 {
  version: typeof SHORTCUT_STORAGE_VERSION;
  overrides: ShortcutOverrides;
}

export type ShortcutMutationFailureReason =
  | ShortcutValidationError
  | "conflict"
  | "invalidIndex"
  | "lastBinding"
  | "maxBindings";

export type ShortcutMutationResult =
  | { ok: true }
  | { ok: false; reason: ShortcutMutationFailureReason; conflictId?: ShortcutId };

export interface ShortcutState {
  /** Sparse user changes. An absent id always resolves to the registry default. */
  overrides: ShortcutOverrides;
  /** Transient capture state; deliberately excluded from persistence. */
  recordingId: ShortcutId | null;
  startRecording: (id: ShortcutId) => void;
  stopRecording: () => void;
  replaceBinding: (
    id: ShortcutId,
    index: number,
    binding: ShortcutBinding,
    platform?: ShortcutPlatformInput,
  ) => ShortcutMutationResult;
  addBinding: (
    id: ShortcutId,
    binding: ShortcutBinding,
    platform?: ShortcutPlatformInput,
  ) => ShortcutMutationResult;
  removeBinding: (
    id: ShortcutId,
    index: number,
    platform?: ShortcutPlatformInput,
  ) => ShortcutMutationResult;
  resetShortcut: (id: ShortcutId, platform?: ShortcutPlatformInput) => ShortcutMutationResult;
  resetAll: () => void;
}

const INVALID_PERSISTED_KEYS = new Set(["Dead", "Process", "Unidentified"]);

function own<T extends object>(value: T, key: PropertyKey): boolean {
  return Object.prototype.hasOwnProperty.call(value, key);
}

function mutableBindings(
  id: ShortcutId,
  overrides: ShortcutOverrides,
  platform: ShortcutPlatformInput,
): ShortcutBinding[] {
  return effectiveShortcutBindings(shortcutById(id), overrides, platform).map((binding) => ({ ...binding }));
}

function sanitizedBinding(raw: unknown): ShortcutBinding | null {
  const binding = sanitizeShortcutBinding(raw);
  if (!binding || INVALID_PERSISTED_KEYS.has(binding.key)) return null;
  return binding;
}

function sparseOverride(
  overrides: ShortcutOverrides,
  id: ShortcutId,
  bindings: readonly ShortcutBinding[],
  platform: ShortcutPlatformInput,
): ShortcutOverrides {
  const next: ShortcutOverrides = { ...overrides };
  const defaults = defaultShortcutBindings(shortcutById(id), platform);
  if (shortcutBindingsEqual(bindings, defaults)) {
    delete next[id];
  } else {
    next[id] = bindings.map((binding) => ({ ...binding }));
  }
  return next;
}

function validationResult(
  id: ShortcutId,
  binding: ShortcutBinding,
  currentBindings: readonly ShortcutBinding[],
  ignoredIndex: number | null,
  overrides: ShortcutOverrides,
  platform: ShortcutPlatformInput,
): ShortcutMutationResult {
  const sanitized = sanitizedBinding(binding);
  if (!sanitized) return { ok: false, reason: "invalidKey" };

  const validationError = validateShortcutBinding(id, sanitized, platform);
  if (validationError) return { ok: false, reason: validationError };

  const duplicate = currentBindings.some(
    (candidate, index) =>
      index !== ignoredIndex && shortcutBindingsConflict(candidate, sanitized, platform),
  );
  if (duplicate) return { ok: false, reason: "conflict", conflictId: id };

  const conflictId = findShortcutConflict(id, sanitized, overrides, platform);
  if (conflictId) return { ok: false, reason: "conflict", conflictId };
  return { ok: true };
}

/**
 * Sanitizes a hand-edited persisted overrides object one entry at a time.
 * Unknown ids, empty arrays, invalid bindings, duplicates, and conflicts are
 * ignored without discarding valid sibling entries.
 */
export function sanitizeShortcutOverrides(
  raw: unknown,
  platform: ShortcutPlatformInput = detectShortcutPlatform(),
): ShortcutOverrides {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return {};
  const source = raw as Record<string, unknown>;
  let candidates: ShortcutOverrides = {};

  // First sanitize every action independently. Conflict checking must wait
  // until the complete effective map exists; otherwise a valid swap (A takes
  // B's default while B takes A's) is incorrectly compared against stale
  // registry defaults and erased during hydration.
  for (const shortcut of SHORTCUTS) {
    if (!own(source, shortcut.id)) continue;
    const rawBindings = source[shortcut.id];
    if (!Array.isArray(rawBindings) || rawBindings.length === 0) continue;

    const bindings: ShortcutBinding[] = [];
    for (const rawBinding of rawBindings.slice(0, MAX_SHORTCUT_BINDINGS)) {
      const binding = sanitizedBinding(rawBinding);
      if (!binding || validateShortcutBinding(shortcut.id, binding, platform)) continue;
      if (bindings.some((candidate) => shortcutBindingsConflict(candidate, binding, platform))) continue;
      bindings.push(binding);
    }

    // An override may never disable a command. If every stored binding was
    // rejected, leave the id absent so the registry default remains active.
    if (bindings.length === 0) continue;
    candidates = sparseOverride(candidates, shortcut.id, bindings, platform);
  }

  // Hand-edited data can still contain cross-action collisions. Drop a
  // conflicting override as a whole (never individual bindings, which could
  // unexpectedly change an action's alternatives), then re-check because
  // restoring its default can reveal a conflict with an earlier override.
  let changed = true;
  while (changed) {
    changed = false;
    for (const shortcut of SHORTCUTS) {
      if (!own(candidates, shortcut.id)) continue;
      const bindings = effectiveShortcutBindings(shortcut, candidates, platform);
      if (bindings.some((binding) => findShortcutConflict(shortcut.id, binding, candidates, platform))) {
        const next: ShortcutOverrides = { ...candidates };
        delete next[shortcut.id];
        candidates = next;
        changed = true;
      }
    }
  }
  return candidates;
}

/** Parses the versioned storage envelope and returns safe sparse overrides. */
export function hydrateShortcutOverrides(
  raw: string | null,
  platform: ShortcutPlatformInput = detectShortcutPlatform(),
): ShortcutOverrides {
  if (!raw) return {};
  try {
    const parsed = JSON.parse(raw) as { version?: unknown; overrides?: unknown } | null;
    if (!parsed || parsed.version !== SHORTCUT_STORAGE_VERSION) return {};
    return sanitizeShortcutOverrides(parsed.overrides, platform);
  } catch {
    return {};
  }
}

function readInitialOverrides(): ShortcutOverrides {
  try {
    return hydrateShortcutOverrides(localStorage.getItem(SHORTCUT_STORAGE_KEY));
  } catch {
    return {};
  }
}

function persist(overrides: ShortcutOverrides): void {
  const payload: PersistedShortcutsV1 = {
    version: SHORTCUT_STORAGE_VERSION,
    overrides,
  };
  try {
    localStorage.setItem(SHORTCUT_STORAGE_KEY, JSON.stringify(payload));
  } catch {
    // Best-effort preference persistence: state changes must still work when
    // storage is unavailable, full, or disabled.
  }
}

const initialOverrides = readInitialOverrides();

export const useShortcutStore = create<ShortcutState>((set, get) => ({
  overrides: initialOverrides,
  recordingId: null,

  startRecording: (id) => set({ recordingId: id }),
  stopRecording: () => set({ recordingId: null }),

  replaceBinding: (id, index, rawBinding, platform = detectShortcutPlatform()) => {
    const state = get();
    const bindings = mutableBindings(id, state.overrides, platform);
    if (!Number.isInteger(index) || index < 0 || index >= bindings.length) {
      return { ok: false, reason: "invalidIndex" };
    }

    const binding = sanitizedBinding(rawBinding);
    if (!binding) return { ok: false, reason: "invalidKey" };
    const result = validationResult(id, binding, bindings, index, state.overrides, platform);
    if (!result.ok) return result;

    bindings[index] = binding;
    const overrides = sparseOverride(state.overrides, id, bindings, platform);
    set({ overrides });
    persist(overrides);
    return { ok: true };
  },

  addBinding: (id, rawBinding, platform = detectShortcutPlatform()) => {
    const state = get();
    const bindings = mutableBindings(id, state.overrides, platform);
    if (bindings.length >= MAX_SHORTCUT_BINDINGS) {
      return { ok: false, reason: "maxBindings" };
    }

    const binding = sanitizedBinding(rawBinding);
    if (!binding) return { ok: false, reason: "invalidKey" };
    const result = validationResult(id, binding, bindings, null, state.overrides, platform);
    if (!result.ok) return result;

    bindings.push(binding);
    const overrides = sparseOverride(state.overrides, id, bindings, platform);
    set({ overrides });
    persist(overrides);
    return { ok: true };
  },

  removeBinding: (id, index, platform = detectShortcutPlatform()) => {
    const state = get();
    const bindings = mutableBindings(id, state.overrides, platform);
    if (!Number.isInteger(index) || index < 0 || index >= bindings.length) {
      return { ok: false, reason: "invalidIndex" };
    }
    if (bindings.length === 1) return { ok: false, reason: "lastBinding" };

    bindings.splice(index, 1);
    const overrides = sparseOverride(state.overrides, id, bindings, platform);
    set({ overrides });
    persist(overrides);
    return { ok: true };
  },

  resetShortcut: (id, platform = detectShortcutPlatform()) => {
    const state = get();
    if (!own(state.overrides, id)) return { ok: true };
    const overrides: ShortcutOverrides = { ...state.overrides };
    delete overrides[id];

    // A different command may have claimed this command's now-free default
    // chord. Refuse the reset instead of silently creating two live actions
    // for the same event; the user can move the conflicting command first.
    for (const binding of defaultShortcutBindings(shortcutById(id), platform)) {
      const conflictId = findShortcutConflict(id, binding, overrides, platform);
      if (conflictId) return { ok: false, reason: "conflict", conflictId };
    }

    set({ overrides });
    persist(overrides);
    return { ok: true };
  },

  resetAll: () => {
    const overrides: ShortcutOverrides = {};
    set({ overrides });
    persist(overrides);
  },
}));

/**
 * Applies a localStorage event emitted by another app window. Exported so the
 * node-only test suite can exercise cross-window synchronization without a DOM.
 */
export function syncShortcutStorageEvent(event: Pick<StorageEvent, "key" | "newValue">): void {
  if (event.key !== SHORTCUT_STORAGE_KEY) return;
  useShortcutStore.setState({ overrides: hydrateShortcutOverrides(event.newValue) });
}

if (typeof window !== "undefined") {
  window.addEventListener("storage", syncShortcutStorageEvent);
}

export default useShortcutStore;
