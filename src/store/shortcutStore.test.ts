import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  MAX_SHORTCUT_BINDINGS,
  SHORTCUT_STORAGE_KEY,
  SHORTCUT_STORAGE_VERSION,
  hydrateShortcutOverrides,
  sanitizeShortcutOverrides,
  syncShortcutStorageEvent,
  useShortcutStore,
} from "./shortcutStore";

class MemoryStorage implements Storage {
  private readonly values = new Map<string, string>();

  get length(): number {
    return this.values.size;
  }

  clear(): void {
    this.values.clear();
  }

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  key(index: number): string | null {
    return [...this.values.keys()][index] ?? null;
  }

  removeItem(key: string): void {
    this.values.delete(key);
  }

  setItem(key: string, value: string): void {
    this.values.set(key, String(value));
  }
}

let storage: MemoryStorage;

beforeEach(() => {
  storage = new MemoryStorage();
  vi.stubGlobal("localStorage", storage);
  useShortcutStore.setState({ overrides: {}, recordingId: null });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function persisted(): { version: number; overrides: Record<string, unknown>; recordingId?: unknown } {
  const raw = storage.getItem(SHORTCUT_STORAGE_KEY);
  expect(raw).not.toBeNull();
  return JSON.parse(raw as string) as {
    version: number;
    overrides: Record<string, unknown>;
    recordingId?: unknown;
  };
}

describe("shortcutStore persistence and mutations", () => {
  it("replaces a binding and persists a sparse versioned override", () => {
    const result = useShortcutStore.getState().replaceBinding(
      "newSession",
      0,
      { key: "k", code: "KeyK", primary: true },
      true,
    );

    expect(result).toEqual({ ok: true });
    expect(useShortcutStore.getState().overrides.newSession).toEqual([
      { key: "k", code: "KeyK", primary: true },
    ]);
    expect(persisted()).toEqual({
      version: SHORTCUT_STORAGE_VERSION,
      overrides: { newSession: [{ key: "k", code: "KeyK", primary: true }] },
    });
  });

  it("adds and removes alternatives without ever removing the last binding", () => {
    expect(
      useShortcutStore.getState().addBinding(
        "newSession",
        { key: "y", code: "KeyY", primary: true },
        true,
      ),
    ).toEqual({ ok: true });
    expect(useShortcutStore.getState().overrides.newSession).toHaveLength(2);

    expect(useShortcutStore.getState().removeBinding("newSession", 0)).toEqual({ ok: true });
    expect(useShortcutStore.getState().overrides.newSession).toEqual([
      { key: "y", code: "KeyY", primary: true },
    ]);

    expect(useShortcutStore.getState().removeBinding("newSession", 0)).toEqual({
      ok: false,
      reason: "lastBinding",
    });
    expect(useShortcutStore.getState().overrides.newSession).toHaveLength(1);
  });

  it("drops an override when replacement returns exactly to the registry default", () => {
    expect(
      useShortcutStore.getState().replaceBinding("newSession", 0, { key: "k", primary: true }, true),
    ).toEqual({ ok: true });
    expect(useShortcutStore.getState().overrides.newSession).toBeDefined();

    expect(
      useShortcutStore.getState().replaceBinding(
        "newSession",
        0,
        { key: "n", code: "KeyN", primary: true },
        true,
      ),
    ).toEqual({ ok: true });
    expect(useShortcutStore.getState().overrides.newSession).toBeUndefined();
    expect(persisted().overrides).toEqual({});
  });

  it("compares edits and resets against the current platform default", () => {
    expect(
      useShortcutStore.getState().replaceBinding(
        "openShortcuts",
        0,
        { key: "/", code: "Slash", primary: true },
        "windows",
      ),
    ).toEqual({ ok: true });
    expect(useShortcutStore.getState().overrides.openShortcuts).toBeUndefined();

    expect(
      useShortcutStore.getState().replaceBinding(
        "openShortcuts",
        0,
        { key: "/", code: "Slash", primary: true },
        "macos",
      ),
    ).toEqual({ ok: true });
    expect(useShortcutStore.getState().overrides.openShortcuts).toEqual([
      { key: "/", code: "Slash", primary: true },
    ]);
    expect(useShortcutStore.getState().resetShortcut("openShortcuts", "macos")).toEqual({ ok: true });
    expect(useShortcutStore.getState().overrides.openShortcuts).toBeUndefined();
  });

  it("returns actionable conflict and validation failures without mutating state", () => {
    expect(
      useShortcutStore.getState().replaceBinding(
        "newSession",
        0,
        { key: ",", code: "Comma", primary: true },
        true,
      ),
    ).toEqual({ ok: false, reason: "conflict", conflictId: "openSettings" });

    expect(
      useShortcutStore.getState().addBinding(
        "newSession",
        { key: "n", code: "KeyN", primary: true },
        true,
      ),
    ).toEqual({ ok: false, reason: "conflict", conflictId: "newSession" });

    expect(
      useShortcutStore.getState().replaceBinding("newSession", 0, { key: "k" }, true),
    ).toEqual({ ok: false, reason: "globalNeedsModifier" });
    expect(useShortcutStore.getState().replaceBinding("newSession", 9, { key: "k", primary: true }, true)).toEqual({
      ok: false,
      reason: "invalidIndex",
    });
    expect(useShortcutStore.getState().overrides).toEqual({});
  });

  it("detects the active platform's primary-modifier aliases", () => {
    expect(
      useShortcutStore.getState().replaceBinding(
        "openSettings",
        0,
        { key: "n", code: "KeyN", meta: true },
        "macos",
      ),
    ).toEqual({ ok: false, reason: "conflict", conflictId: "newSession" });
    expect(
      useShortcutStore.getState().replaceBinding(
        "openSettings",
        0,
        { key: "n", code: "KeyN", control: true },
        "windows",
      ),
    ).toEqual({ ok: false, reason: "conflict", conflictId: "newSession" });
  });

  it("caps the number of alternative bindings", () => {
    for (const key of ["g", "h", "i"]) {
      expect(useShortcutStore.getState().addBinding("sessionDelete", { key }, true)).toEqual({ ok: true });
    }
    expect(useShortcutStore.getState().overrides.sessionDelete).toHaveLength(MAX_SHORTCUT_BINDINGS);
    expect(useShortcutStore.getState().addBinding("sessionDelete", { key: "j" }, true)).toEqual({
      ok: false,
      reason: "maxBindings",
    });
  });

  it("resets one shortcut or all shortcuts back to sparse defaults", () => {
    expect(
      useShortcutStore.getState().replaceBinding("newSession", 0, { key: "k", primary: true }, true),
    ).toEqual({ ok: true });
    expect(
      useShortcutStore.getState().replaceBinding("openSettings", 0, { key: "j", primary: true }, true),
    ).toEqual({ ok: true });

    expect(useShortcutStore.getState().resetShortcut("newSession")).toEqual({ ok: true });
    expect(useShortcutStore.getState().overrides).toEqual({
      openSettings: [{ key: "j", primary: true }],
    });

    useShortcutStore.getState().resetAll();
    expect(useShortcutStore.getState().overrides).toEqual({});
    expect(persisted().overrides).toEqual({});
  });

  it("refuses a per-action reset that would reclaim a chord another action now uses", () => {
    expect(
      useShortcutStore.getState().replaceBinding("newSession", 0, { key: "k", primary: true }, true),
    ).toEqual({ ok: true });
    expect(
      useShortcutStore.getState().replaceBinding(
        "openSettings",
        0,
        { key: "n", code: "KeyN", primary: true },
        true,
      ),
    ).toEqual({ ok: true });

    expect(useShortcutStore.getState().resetShortcut("newSession")).toEqual({
      ok: false,
      reason: "conflict",
      conflictId: "openSettings",
    });
    expect(useShortcutStore.getState().overrides).toEqual({
      newSession: [{ key: "k", primary: true }],
      openSettings: [{ key: "n", code: "KeyN", primary: true }],
    });
  });

  it("allows resetting an intentional contextual default overlap", () => {
    expect(
      useShortcutStore.getState().replaceBinding("sessionCloseMenu", 0, { key: "F10" }, true),
    ).toEqual({ ok: true });
    expect(useShortcutStore.getState().resetShortcut("sessionCloseMenu")).toEqual({ ok: true });
    expect(useShortcutStore.getState().overrides.sessionCloseMenu).toBeUndefined();
  });

  it("keeps recording state transient", () => {
    useShortcutStore.getState().resetAll();
    const before = storage.getItem(SHORTCUT_STORAGE_KEY);

    useShortcutStore.getState().startRecording("newSession");
    expect(useShortcutStore.getState().recordingId).toBe("newSession");
    expect(storage.getItem(SHORTCUT_STORAGE_KEY)).toBe(before);
    expect(persisted().recordingId).toBeUndefined();

    useShortcutStore.getState().stopRecording();
    expect(useShortcutStore.getState().recordingId).toBeNull();
    expect(storage.getItem(SHORTCUT_STORAGE_KEY)).toBe(before);
  });
});

describe("shortcutStore hydration", () => {
  it("returns defaults for absent, corrupt, and unknown-version payloads", () => {
    expect(hydrateShortcutOverrides(null, true)).toEqual({});
    expect(hydrateShortcutOverrides("not-json", true)).toEqual({});
    expect(hydrateShortcutOverrides(JSON.stringify({ version: 2, overrides: {} }), true)).toEqual({});
  });

  it("sanitizes entries independently and never accepts an empty override", () => {
    const overrides = sanitizeShortcutOverrides(
      {
        newSession: [
          { key: "K", code: "KeyK", primary: true },
          { key: "k", primary: true },
          { key: "Dead", primary: true },
          null,
        ],
        openSettings: [],
        sessionOpenSplit: "not-an-array",
        removedShortcut: [{ key: "x" }],
      },
      true,
    );

    expect(overrides).toEqual({
      newSession: [{ key: "k", code: "KeyK", primary: true }],
    });
    expect(Object.values(overrides).every((bindings) => bindings && bindings.length > 0)).toBe(true);
  });

  it("omits default-equivalent and conflicting persisted entries", () => {
    const raw = JSON.stringify({
      version: SHORTCUT_STORAGE_VERSION,
      overrides: {
        newSession: [{ key: ",", code: "Comma", primary: true }],
        openSettings: [{ key: ",", code: "Comma", primary: true }],
      },
    });

    expect(hydrateShortcutOverrides(raw, true)).toEqual({});
  });

  it("sanitizes sparse overrides against the platform-specific defaults", () => {
    const raw = JSON.stringify({
      version: SHORTCUT_STORAGE_VERSION,
      overrides: {
        openShortcuts: [{ key: "/", code: "Slash", primary: true }],
      },
    });

    expect(hydrateShortcutOverrides(raw, "windows")).toEqual({});
    expect(hydrateShortcutOverrides(raw, "linux")).toEqual({});
    expect(hydrateShortcutOverrides(raw, "macos")).toEqual({
      openShortcuts: [{ key: "/", code: "Slash", primary: true }],
    });
  });

  it("preserves a valid full-map swap instead of comparing against stale defaults", () => {
    const raw = JSON.stringify({
      version: SHORTCUT_STORAGE_VERSION,
      overrides: {
        newSession: [{ key: ",", code: "Comma", primary: true }],
        openSettings: [{ key: "n", code: "KeyN", primary: true }],
      },
    });

    expect(hydrateShortcutOverrides(raw, true)).toEqual({
      newSession: [{ key: ",", code: "Comma", primary: true }],
      openSettings: [{ key: "n", code: "KeyN", primary: true }],
    });
  });
});

describe("shortcutStore storage-event sync", () => {
  it("applies only this store's events and preserves transient recording state", () => {
    useShortcutStore.setState({
      overrides: { newSession: [{ key: "k", primary: true }] },
      recordingId: "openSettings",
    });

    syncShortcutStorageEvent({
      key: "unrelated-key",
      newValue: JSON.stringify({ version: SHORTCUT_STORAGE_VERSION, overrides: {} }),
    });
    expect(useShortcutStore.getState().overrides.newSession).toBeDefined();

    syncShortcutStorageEvent({
      key: SHORTCUT_STORAGE_KEY,
      newValue: JSON.stringify({
        version: SHORTCUT_STORAGE_VERSION,
        overrides: { openSettings: [{ key: "j", primary: true }] },
      }),
    });
    expect(useShortcutStore.getState()).toMatchObject({
      overrides: { openSettings: [{ key: "j", primary: true }] },
      recordingId: "openSettings",
    });

    syncShortcutStorageEvent({ key: SHORTCUT_STORAGE_KEY, newValue: null });
    expect(useShortcutStore.getState()).toMatchObject({ overrides: {}, recordingId: "openSettings" });
  });
});
