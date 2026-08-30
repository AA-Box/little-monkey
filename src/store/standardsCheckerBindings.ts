const STORAGE_KEY = "little-monkey-standards-checker-bindings-v1";

export type StandardsCheckerBindings = Record<string, string[]>;
type StoredBindings = Record<string, StandardsCheckerBindings>;

function normalize(ids: readonly string[]): string[] {
  return [...new Set(ids.map((id) => id.trim()).filter(Boolean))].sort();
}

function readAll(): StoredBindings {
  if (typeof localStorage === "undefined") return {};
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "{}");
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    const output: StoredBindings = {};
    for (const [workspace, rawBindings] of Object.entries(parsed as Record<string, unknown>)) {
      if (!rawBindings || typeof rawBindings !== "object" || Array.isArray(rawBindings)) continue;
      const bindings: StandardsCheckerBindings = {};
      for (const [standardId, rawIds] of Object.entries(rawBindings as Record<string, unknown>)) {
        if (!Array.isArray(rawIds) || !rawIds.every((id) => typeof id === "string")) continue;
        const ids = normalize(rawIds);
        if (ids.length > 0) bindings[standardId] = ids;
      }
      output[workspace] = bindings;
    }
    return output;
  } catch {
    return {};
  }
}

function persist(all: StoredBindings): void {
  if (typeof localStorage === "undefined") return;
  localStorage.setItem(STORAGE_KEY, JSON.stringify(all));
}

export function loadStandardsCheckerBindings(workspacePath: string): StandardsCheckerBindings {
  return structuredClone(readAll()[workspacePath] ?? {});
}

export function saveStandardCheckerBinding(
  workspacePath: string,
  standardId: string,
  commandIds: readonly string[],
): StandardsCheckerBindings {
  const all = readAll();
  const bindings = { ...(all[workspacePath] ?? {}) };
  const ids = normalize(commandIds);
  if (ids.length > 0) bindings[standardId] = ids;
  else delete bindings[standardId];
  all[workspacePath] = bindings;
  persist(all);
  return structuredClone(bindings);
}

export function pruneStandardsCheckerBindings(
  workspacePath: string,
  validStandardIds: readonly string[],
): StandardsCheckerBindings {
  const all = readAll();
  const valid = new Set(validStandardIds);
  const bindings = { ...(all[workspacePath] ?? {}) };
  let changed = false;
  for (const standardId of Object.keys(bindings)) {
    if (valid.has(standardId)) continue;
    delete bindings[standardId];
    changed = true;
  }
  if (changed) {
    all[workspacePath] = bindings;
    persist(all);
  }
  return structuredClone(bindings);
}
