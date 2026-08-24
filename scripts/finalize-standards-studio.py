from __future__ import annotations

from pathlib import Path
import re
import textwrap

ROOT = Path(__file__).resolve().parents[1]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, content: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content, encoding="utf-8")


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected exactly one match, found {count}: {old[:120]!r}")
    write(path, text.replace(old, new, 1))


def replace_count(path: str, old: str, new: str, expected: int) -> None:
    text = read(path)
    count = text.count(old)
    if count != expected:
        raise RuntimeError(f"{path}: expected {expected} matches, found {count}: {old[:120]!r}")
    write(path, text.replace(old, new))


def regex_once(path: str, pattern: str, replacement: str, flags: int = 0) -> None:
    text = read(path)
    updated, count = re.subn(pattern, replacement, text, count=1, flags=flags)
    if count != 1:
        raise RuntimeError(f"{path}: expected one regex match, found {count}: {pattern[:120]!r}")
    write(path, updated)


# ---------------------------------------------------------------------------
# Portable standards are policy only. Repository JSON must never carry an
# executable Verification binding from one machine to another.
# ---------------------------------------------------------------------------
replace_once(
    "src/lib/standards.ts",
    '  reason: "rediscovered" | "approved_revision" | "imported_revision";\n',
    '  reason: "rediscovered" | "approved_revision" | "imported_revision" | "rejected_revision";\n',
)
replace_once(
    "src/lib/standards.ts",
    '  /** IDs only. The command text is intentionally NOT stored in the repository;\n   * execution resolves these IDs through the app-owned Verification config. */\n  checker_command_ids: string[];\n',
    '',
)
replace_once(
    "src/lib/standards.ts",
    '  if (!Array.isArray(standard.revision_history)) standard.revision_history = [];\n  if (standard.pending_revision === undefined) standard.pending_revision = null;\n  if (!Array.isArray(standard.checker_command_ids)) standard.checker_command_ids = [];\n\n  for (const revision of standard.revision_history) validateRevision(standard.standard_id, revision);\n  if (standard.pending_revision) validateRevision(standard.standard_id, standard.pending_revision);\n  if (!standard.checker_command_ids.every((id) => typeof id === "string" && id.trim().length > 0)) {\n    throw new Error(`Standard ${standard.standard_id} has malformed checker command ids.`);\n  }\n',
    '  if (!Array.isArray(standard.revision_history)) standard.revision_history = [];\n  if (standard.pending_revision === undefined) standard.pending_revision = null;\n\n  // Schema v1 briefly persisted Verification command ids in repository JSON.\n  // Treat that field as legacy/untrusted input and erase it while loading so a\n  // clone/import can never synthesize local process-execution authority.\n  const legacy = standard as Partial<EngineeringStandard> & { checker_command_ids?: unknown };\n  if ("checker_command_ids" in legacy) delete legacy.checker_command_ids;\n\n  for (const revision of standard.revision_history) validateRevision(standard.standard_id, revision);\n  if (standard.pending_revision) validateRevision(standard.standard_id, standard.pending_revision);\n',
)
replace_once(
    "src/lib/standards.ts",
    '  return standards.map((standard) => conflicting.has(standard.standard_id) && !["rejected", "deprecated", "stale"].includes(standard.status)\n    ? { ...standard, status: "conflicting" as const }\n    : standard);\n',
    '  return standards.map((standard) => {\n    if (["rejected", "deprecated", "stale"].includes(standard.status)) return standard;\n    if (conflicting.has(standard.standard_id)) return { ...standard, status: "conflicting" as const };\n    // `conflicting` is a derived lifecycle state. Once the other active side is\n    // resolved, restore the locally-authoritative state deterministically.\n    if (standard.status === "conflicting") {\n      return { ...standard, status: standard.approved_at_ms === null ? "candidate" as const : "approved" as const };\n    }\n    return standard;\n  });\n',
)
replace_once(
    "src/lib/standards.ts",
    '      standard.evidence.length > 0\n        ? `Evidence: ${standard.evidence.slice(0, 5).map((evidence) => `${evidence.supports ? "+" : "-"}${evidence.path}${evidence.line ? `:${evidence.line}` : ""}@${evidence.sha256.slice(0, 12)}`).join(", ")}.`\n        : "Evidence: manual/imported standard with no repository evidence rows.",\n      standard.checker_command_ids.length > 0\n        ? `Mechanical verification: ${standard.checker_command_ids.length} locally-bound Verification command${standard.checker_command_ids.length === 1 ? "" : "s"}; command text is intentionally not stored in repository policy.`\n        : "Mechanical verification: no local Verification command bound.",\n',
    '      standard.evidence.length > 0\n        ? `Evidence: ${standard.evidence.slice(0, 5).map((evidence) => `${evidence.supports ? "+" : "-"}${evidence.path}${evidence.line ? `:${evidence.line}` : ""}@${evidence.sha256.slice(0, 12)}`).join(", ")}.`\n        : "Evidence: manual/imported standard with no repository evidence rows.",\n',
)
replace_count(
    "src/lib/standards.ts",
    '        checker_command_ids: [...existing.checker_command_ids],\n',
    '',
    1,
)

# ---------------------------------------------------------------------------
# Repository lifecycle: read-only drift evaluation, explicit persistence,
# untrusted imports, recomputed digests, and pending-revision rejection.
# ---------------------------------------------------------------------------
replace_once(
    "src/lib/standardsRepository.ts",
    '  emptyStandardsDocument,\n  mergeDiscoveredStandards,\n',
    '  detectStandardConflicts,\n  emptyStandardsDocument,\n  mergeDiscoveredStandards,\n',
)
replace_once(
    "src/lib/standardsRepository.ts",
    '    revision_history: [],\n    pending_revision: null,\n    checker_command_ids: [],\n',
    '    revision_history: [],\n    pending_revision: null,\n',
)
replace_once(
    "src/lib/standardsRepository.ts",
    '  const next = { ...current, standards: mergeDiscoveredStandards(current.standards, discovered), generated_at_ms: Date.now() };\n',
    '  const next = { ...current, standards: detectStandardConflicts(mergeDiscoveredStandards(current.standards, discovered)), generated_at_ms: Date.now() };\n',
)
regex_once(
    "src/lib/standardsRepository.ts",
    r'export async function checkStandardsDrift\(workspacePath: string, document: StandardsDocument\): Promise<StandardsDocument> \{.*?\n\}\n\nfunction activatePendingRevision',
    textwrap.dedent('''\
    export async function evaluateStandardsDrift(document: StandardsDocument): Promise<StandardsDocument> {
      const now = Date.now();
      const standards: EngineeringStandard[] = [];
      for (const standard of document.standards) {
        const shouldEvaluate = (standard.status === "approved" || standard.status === "stale" || standard.status === "conflicting")
          && standard.evidence.some((item) => item.supports);
        if (!shouldEvaluate) {
          standards.push(standard.pending_revision && standard.drift === "healthy"
            ? { ...standard, drift: "weakened" }
            : standard);
          continue;
        }
        let supporting = 0;
        let changed = 0;
        let missing = 0;
        for (const evidence of standard.evidence.filter((item) => item.supports)) {
          const current = await readWorkspaceText(evidence.path);
          if (current === null) { missing += 1; continue; }
          if ((await sha256(current)) === evidence.sha256) supporting += 1;
          else changed += 1;
        }
        let drift: EngineeringStandard["drift"] = supporting > 0 && changed === 0 && missing === 0
          ? "healthy"
          : supporting === 0 && (changed > 0 || missing > 0)
            ? "contradicted"
            : "weakened";
        if (standard.pending_revision && drift === "healthy") drift = "weakened";
        let status = standard.status;
        if (drift === "contradicted" && status === "approved") status = "stale";
        else if (status === "stale" && drift !== "contradicted") status = "approved";
        standards.push({ ...standard, drift, status, last_verified_at_ms: now });
      }
      return { ...document, standards: detectStandardConflicts(standards), generated_at_ms: now };
    }

    export async function checkStandardsDrift(workspacePath: string, document: StandardsDocument): Promise<StandardsDocument> {
      const next = await evaluateStandardsDrift(document);
      await saveStandards(workspacePath, next);
      return next;
    }

    function activatePendingRevision'''),
    flags=re.S,
)
replace_once(
    "src/lib/standardsRepository.ts",
    '  const next = { ...document, standards, generated_at_ms: now };\n  await saveStandards(workspacePath, next);\n  return next;\n}\n\nexport async function setStandardStatus',
    '  const next = { ...document, standards: detectStandardConflicts(standards), generated_at_ms: now };\n  await saveStandards(workspacePath, next);\n  return next;\n}\n\nexport async function rejectPendingRevision(workspacePath: string, document: StandardsDocument, standardId: string): Promise<StandardsDocument> {\n  const now = Date.now();\n  const standards = document.standards.map((standard) => {\n    if (standard.standard_id !== standardId || !standard.pending_revision) return standard;\n    const pending = standard.pending_revision;\n    return {\n      ...standard,\n      pending_revision: null,\n      revision_history: [...standard.revision_history, {\n        version: pending.version,\n        title: pending.title,\n        body: pending.body,\n        applicability: structuredClone(pending.applicability),\n        severity: pending.severity,\n        tags: [...pending.tags],\n        evidence: pending.evidence.map((entry) => ({ ...entry })),\n        content_sha256: pending.content_sha256,\n        recorded_at_ms: now,\n        reason: "rejected_revision" as const,\n      }],\n    };\n  });\n  const evaluated = await evaluateStandardsDrift({ ...document, standards: detectStandardConflicts(standards), generated_at_ms: now });\n  await saveStandards(workspacePath, evaluated);\n  return evaluated;\n}\n\nexport async function setStandardStatus',
)
replace_once(
    "src/lib/standardsRepository.ts",
    '  const standards = document.standards.map((standard) => standard.standard_id === standardId ? { ...standard, status } : standard);\n  const next = { ...document, standards, generated_at_ms: Date.now() };\n',
    '  const standards = detectStandardConflicts(document.standards.map((standard) => standard.standard_id === standardId ? { ...standard, status } : standard));\n  const next = { ...document, standards, generated_at_ms: Date.now() };\n',
)
regex_once(
    "src/lib/standardsRepository.ts",
    r'\nexport async function setStandardCheckers\(workspacePath: string, document: StandardsDocument, standardId: string, commandIds: string\[\]\): Promise<StandardsDocument> \{.*?\n\}\n',
    '\n',
    flags=re.S,
)
regex_once(
    "src/lib/standardsRepository.ts",
    r'export async function importStandards\(workspacePath: string, sourcePath: string\): Promise<StandardsDocument> \{.*?\n\}\n\nexport async function exportStandards',
    textwrap.dedent('''\
    export async function importStandards(workspacePath: string, sourcePath: string): Promise<StandardsDocument> {
      const raw = await readWorkspaceText(sourcePath);
      if (!raw) throw new Error(`Could not read ${sourcePath} from the active workspace.`);
      const incoming = validateStandardsDocument(JSON.parse(raw));
      const current = await loadStandards(workspacePath);
      const byId = new Map(current.standards.map((standard) => [standard.standard_id, standard]));
      for (const imported of incoming.standards) {
        const digest = await contentDigest(imported.title, imported.body, imported.applicability, imported.severity, imported.tags);
        const normalized: EngineeringStandard = {
          ...imported,
          origin: "imported",
          status: "candidate",
          approved_at_ms: null,
          content_sha256: digest,
          revision_history: [],
          pending_revision: null,
        };
        const existing = byId.get(normalized.standard_id);
        if (!existing) {
          byId.set(normalized.standard_id, normalized);
          continue;
        }
        if (existing.content_sha256 === digest) {
          // Identical portable policy may refresh evidence only. Local approval,
          // lifecycle/history, and conflicts remain authoritative on this device.
          byId.set(normalized.standard_id, {
            ...existing,
            evidence: normalized.evidence.map((entry) => ({ ...entry })),
            last_verified_at_ms: Date.now(),
          });
          continue;
        }
        const locallyAuthoritative = existing.approved_at_ms !== null && existing.status !== "rejected";
        if (locallyAuthoritative) {
          const now = Date.now();
          byId.set(normalized.standard_id, {
            ...existing,
            drift: existing.drift === "contradicted" ? "contradicted" : "weakened",
            pending_revision: {
              version: existing.version + 1,
              title: normalized.title,
              body: normalized.body,
              applicability: structuredClone(normalized.applicability),
              severity: normalized.severity,
              tags: [...normalized.tags],
              evidence: normalized.evidence.map((entry) => ({ ...entry })),
              content_sha256: digest,
              recorded_at_ms: now,
              proposed_at_ms: now,
              source: "imported",
            },
          });
        } else {
          byId.set(normalized.standard_id, {
            ...normalized,
            version: existing.version + 1,
            created_at_ms: existing.created_at_ms,
            revision_history: [...existing.revision_history, snapshotStandardRevision(existing, "imported_revision")],
          });
        }
      }
      const next = { ...current, standards: detectStandardConflicts([...byId.values()]), generated_at_ms: Date.now() };
      await saveStandards(workspacePath, next);
      return next;
    }

    export async function exportStandards'''),
    flags=re.S,
)

# ---------------------------------------------------------------------------
# App-local checker bindings. The repository carries no command ids at all.
# ---------------------------------------------------------------------------
write(
    "src/store/standardsCheckerBindings.ts",
    textwrap.dedent('''\
    const STORAGE_KEY = "little-monkey-standards-checker-bindings-v1";

    export type StandardsCheckerBindings = Record<string, string[]>;
    type StoredBindings = Record<string, StandardsCheckerBindings>;

    function normalize(ids: readonly string[]): string[] {
      return [...new Set(ids.map((id) => id.trim()).filter(Boolean))].sort();
    }

    function readAll(): StoredBindings {
      if (typeof localStorage === "undefined") return {};
      try {
        const parsed = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "{}");
        if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
        const output: StoredBindings = {};
        for (const [workspace, rawBindings] of Object.entries(parsed as Record<string, unknown>)) {
          if (!rawBindings || typeof rawBindings !== "object" || Array.isArray(rawBindings)) continue;
          const bindings: StandardsCheckerBindings = {};
          for (const [standardId, rawIds] of Object.entries(rawBindings as Record<string, unknown>)) {
            if (!Array.isArray(rawIds) || !rawIds.every((id) => typeof id === "string")) continue;
            const ids = normalize(rawIds as string[]);
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

    export function saveStandardCheckerBinding(workspacePath: string, standardId: string, commandIds: readonly string[]): StandardsCheckerBindings {
      const all = readAll();
      const bindings = { ...(all[workspacePath] ?? {}) };
      const normalized = normalize(commandIds);
      if (normalized.length > 0) bindings[standardId] = normalized;
      else delete bindings[standardId];
      all[workspacePath] = bindings;
      persist(all);
      return structuredClone(bindings);
    }

    export function pruneStandardsCheckerBindings(workspacePath: string, validStandardIds: readonly string[]): StandardsCheckerBindings {
      const valid = new Set(validStandardIds);
      const all = readAll();
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
    '''),
)

write(
    "src/lib/standardsTaskContext.ts",
    textwrap.dedent('''\
    import { standardsPromptSection } from "./standards";
    import { useStandardsStore } from "../store/standardsStore";

    export interface FrozenStandardsTaskContext {
      taskText: string;
      fileHints: string[];
      section: string;
      checkerCommandIds: string[];
    }

    interface PathLike { path: string }

    export async function freezeStandardsTaskContext(
      taskText: string,
      textRefs: readonly PathLike[] = [],
      images: readonly PathLike[] = [],
    ): Promise<FrozenStandardsTaskContext> {
      const frozenTask = taskText.trim();
      const fileHints = [...new Set([...textRefs, ...images].map((item) => item.path.trim()).filter(Boolean))].sort();
      await useStandardsStore.getState().refresh();
      const state = useStandardsStore.getState();
      const selection = state.preview(frozenTask, fileHints);
      const checkerCommandIds = [...new Set(selection.selected.flatMap(({ standard }) => state.checkerBindings[standard.standard_id] ?? []))].sort();
      return {
        taskText: frozenTask,
        fileHints,
        section: standardsPromptSection(selection),
        checkerCommandIds,
      };
    }
    '''),
)

# Replace the store wholesale; this preserves its public actions while moving
# checker authority out of repository data and making refresh drift read-only.
write(
    "src/store/standardsStore.ts",
    textwrap.dedent('''\
    import { invoke } from "@tauri-apps/api/core";
    import { create } from "zustand";

    import { primaryRoot, useWorkspaceStore } from "./workspaceStore";
    import { useVerifyStore } from "./verifyStore";
    import {
      DEFAULT_STANDARDS_CHAR_BUDGET,
      emptyStandardsDocument,
      selectStandards,
      type StandardsDocument,
      type StandardsSelection,
    } from "../lib/standards";
    import {
      approveStandard,
      checkStandardsDrift,
      discoverAndMergeStandards,
      evaluateStandardsDrift,
      exportAgentOsStandards,
      exportStandards,
      importAgentOsStandards,
      importStandards,
      loadStandards,
      rejectPendingRevision,
      setStandardStatus,
    } from "../lib/standardsRepository";
    import {
      loadStandardsCheckerBindings,
      pruneStandardsCheckerBindings,
      saveStandardCheckerBinding,
      type StandardsCheckerBindings,
    } from "./standardsCheckerBindings";
    import type { VerifyResult } from "./verifyTypes";

    export const STANDARDS_IMPORT_PATH = ".little-monkey/standards/import.json";
    export const STANDARDS_EXPORT_PATH = ".little-monkey/standards/export.json";

    interface StandardsStore {
      document: StandardsDocument | null;
      workspacePath: string | null;
      checkerBindings: StandardsCheckerBindings;
      loading: boolean;
      error: string | null;
      lastExportPath: string | null;
      lastCheckerResults: Record<string, VerifyResult[]>;
      refresh: () => Promise<void>;
      discover: () => Promise<void>;
      approve: (standardId: string) => Promise<void>;
      reject: (standardId: string) => Promise<void>;
      rejectRevision: (standardId: string) => Promise<void>;
      deprecate: (standardId: string) => Promise<void>;
      drift: () => Promise<void>;
      preview: (taskText: string, fileHints?: string[], budgetChars?: number) => StandardsSelection;
      importFile: () => Promise<void>;
      exportFile: () => Promise<void>;
      importAgentOs: () => Promise<void>;
      exportAgentOs: () => Promise<void>;
      setCheckers: (standardId: string, commandIds: string[]) => Promise<void>;
      runCheckers: (standardId: string) => Promise<VerifyResult[]>;
    }

    function activeWorkspace(): string | null {
      return primaryRoot(useWorkspaceStore.getState().roots)?.path ?? null;
    }

    function message(error: unknown): string {
      return error instanceof Error ? error.message : String(error);
    }

    let refreshSequence = 0;

    export const useStandardsStore = create<StandardsStore>((set, get) => ({
      document: null,
      workspacePath: null,
      checkerBindings: {},
      loading: false,
      error: null,
      lastExportPath: null,
      lastCheckerResults: {},

      refresh: async () => {
        const sequence = ++refreshSequence;
        const workspacePath = activeWorkspace();
        if (!workspacePath) {
          set({ document: null, workspacePath: null, checkerBindings: {}, loading: false, error: null, lastExportPath: null, lastCheckerResults: {} });
          return;
        }
        set({ loading: true, error: null });
        try {
          const loaded = await loadStandards(workspacePath);
          const document = await evaluateStandardsDrift(loaded);
          const checkerBindings = pruneStandardsCheckerBindings(workspacePath, document.standards.map((standard) => standard.standard_id));
          if (sequence !== refreshSequence) return;
          set({ document, workspacePath, checkerBindings, loading: false, error: null });
        } catch (error) {
          if (sequence !== refreshSequence) return;
          set({ document: emptyStandardsDocument(workspacePath), workspacePath, checkerBindings: loadStandardsCheckerBindings(workspacePath), loading: false, error: message(error) });
        }
      },

      discover: async () => {
        const workspacePath = activeWorkspace();
        if (!workspacePath) throw new Error("Open a workspace before discovering standards.");
        set({ loading: true, error: null });
        try {
          const document = await discoverAndMergeStandards(workspacePath);
          const checkerBindings = pruneStandardsCheckerBindings(workspacePath, document.standards.map((standard) => standard.standard_id));
          set({ document, workspacePath, checkerBindings, loading: false });
        } catch (error) {
          set({ loading: false, error: message(error) });
          throw error;
        }
      },

      approve: async (standardId) => {
        const workspacePath = activeWorkspace();
        const document = get().document;
        if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
        set({ document: await approveStandard(workspacePath, document, standardId), error: null });
      },

      reject: async (standardId) => {
        const workspacePath = activeWorkspace();
        const document = get().document;
        if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
        set({ document: await setStandardStatus(workspacePath, document, standardId, "rejected"), error: null });
      },

      rejectRevision: async (standardId) => {
        const workspacePath = activeWorkspace();
        const document = get().document;
        if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
        set({ document: await rejectPendingRevision(workspacePath, document, standardId), error: null });
      },

      deprecate: async (standardId) => {
        const workspacePath = activeWorkspace();
        const document = get().document;
        if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
        set({ document: await setStandardStatus(workspacePath, document, standardId, "deprecated"), error: null });
      },

      drift: async () => {
        const workspacePath = activeWorkspace();
        const document = get().document;
        if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
        set({ loading: true, error: null });
        try {
          set({ document: await checkStandardsDrift(workspacePath, document), loading: false });
        } catch (error) {
          set({ loading: false, error: message(error) });
          throw error;
        }
      },

      preview: (taskText, fileHints = [], budgetChars = DEFAULT_STANDARDS_CHAR_BUDGET) =>
        selectStandards(get().document?.standards ?? [], taskText, fileHints, budgetChars),

      importFile: async () => {
        const workspacePath = activeWorkspace();
        if (!workspacePath) throw new Error("Open a workspace before importing standards.");
        set({ loading: true, error: null });
        try {
          const document = await importStandards(workspacePath, STANDARDS_IMPORT_PATH);
          const checkerBindings = pruneStandardsCheckerBindings(workspacePath, document.standards.map((standard) => standard.standard_id));
          set({ document, workspacePath, checkerBindings, loading: false });
        } catch (error) {
          set({ loading: false, error: `${message(error)} Put the portable JSON at ${STANDARDS_IMPORT_PATH} and retry.` });
          throw error;
        }
      },

      exportFile: async () => {
        const document = get().document;
        if (!document) throw new Error("No standards are loaded.");
        set({ loading: true, error: null });
        try {
          const path = await exportStandards(document);
          set({ loading: false, lastExportPath: path });
        } catch (error) {
          set({ loading: false, error: message(error) });
          throw error;
        }
      },

      importAgentOs: async () => {
        const workspacePath = activeWorkspace();
        if (!workspacePath) throw new Error("Open a workspace before importing Agent OS standards.");
        set({ loading: true, error: null });
        try {
          const document = await importAgentOsStandards(workspacePath);
          const checkerBindings = pruneStandardsCheckerBindings(workspacePath, document.standards.map((standard) => standard.standard_id));
          set({ document, workspacePath, checkerBindings, loading: false });
        } catch (error) {
          set({ loading: false, error: message(error) });
          throw error;
        }
      },

      exportAgentOs: async () => {
        const document = get().document;
        if (!document) throw new Error("No standards are loaded.");
        set({ loading: true, error: null });
        try {
          const path = await exportAgentOsStandards(document);
          set({ loading: false, lastExportPath: path });
        } catch (error) {
          set({ loading: false, error: message(error) });
          throw error;
        }
      },

      setCheckers: async (standardId, commandIds) => {
        const workspacePath = activeWorkspace();
        const document = get().document;
        if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
        if (!document.standards.some((standard) => standard.standard_id === standardId)) throw new Error(`Unknown standard ${standardId}.`);
        await useVerifyStore.getState().refresh();
        const enabled = new Set(useVerifyStore.getState().config.commands.filter((command) => command.enabled).map((command) => command.id));
        const normalized = [...new Set(commandIds.map((id) => id.trim()).filter(Boolean))];
        const unavailable = normalized.filter((id) => !enabled.has(id));
        if (unavailable.length > 0) throw new Error(`Verification command is disabled or missing: ${unavailable.join(", ")}.`);
        const checkerBindings = saveStandardCheckerBinding(workspacePath, standardId, normalized);
        set({ checkerBindings, error: null });
      },

      runCheckers: async (standardId) => {
        const workspacePath = activeWorkspace();
        const standard = get().document?.standards.find((entry) => entry.standard_id === standardId);
        if (!workspacePath || !standard) throw new Error(`Unknown standard ${standardId}.`);
        const commandIds = get().checkerBindings[standardId] ?? [];
        if (commandIds.length === 0) throw new Error("No Verification commands are bound to this standard.");
        await useVerifyStore.getState().refresh();
        const enabled = new Map(useVerifyStore.getState().config.commands.filter((command) => command.enabled).map((command) => [command.id, command]));
        const unavailable = commandIds.filter((id) => !enabled.has(id));
        if (unavailable.length > 0) throw new Error(`Bound Verification command is disabled or missing: ${unavailable.join(", ")}.`);
        const results: VerifyResult[] = [];
        for (const commandId of commandIds) {
          results.push(await invoke<VerifyResult>("verify_run", { commandId, turnId: null, sandboxPath: null }));
        }
        set((state) => ({ lastCheckerResults: { ...state.lastCheckerResults, [standardId]: results }, error: null }));
        return results;
      },
    }));
    '''),
)

# UI reads local checker bindings, and pending revisions can be rejected without
# rejecting/deprecating the active approved policy.
replace_once(
    "src/components/Settings/StandardsStudioPanel.tsx",
    '  const reject = useStandardsStore((state) => state.reject);\n  const deprecate = useStandardsStore((state) => state.deprecate);\n',
    '  const reject = useStandardsStore((state) => state.reject);\n  const rejectRevision = useStandardsStore((state) => state.rejectRevision);\n  const deprecate = useStandardsStore((state) => state.deprecate);\n',
)
replace_once(
    "src/components/Settings/StandardsStudioPanel.tsx",
    '  const checkerResults = useStandardsStore((state) => state.lastCheckerResults[standard.standard_id] ?? []);\n',
    '  const checkerResults = useStandardsStore((state) => state.lastCheckerResults[standard.standard_id] ?? []);\n  const checkerIds = useStandardsStore((state) => state.checkerBindings[standard.standard_id] ?? []);\n',
)
replace_count(
    "src/components/Settings/StandardsStudioPanel.tsx",
    'standard.checker_command_ids',
    'checkerIds',
    5,
)
replace_once(
    "src/components/Settings/StandardsStudioPanel.tsx",
    '          <p className="mt-1 whitespace-pre-wrap text-muted">{standard.pending_revision.body}</p>\n          <p className="mt-1 font-mono text-[10px] text-faint">sha256:{standard.pending_revision.content_sha256}</p>\n',
    '          <p className="mt-1 whitespace-pre-wrap text-muted">{standard.pending_revision.body}</p>\n          <p className="mt-1 font-mono text-[10px] text-faint">sha256:{standard.pending_revision.content_sha256}</p>\n          <div className="mt-2 flex gap-2">\n            <Button size="sm" variant="primary" disabled={busy} onClick={() => void run(() => approve(standard.standard_id))}><Check size={13} /> Approve revision</Button>\n            <Button size="sm" disabled={busy} onClick={() => void run(() => rejectRevision(standard.standard_id))}><X size={13} /> Reject revision</Button>\n          </div>\n',
)

# ---------------------------------------------------------------------------
# Exact task scoping + frozen standards selection for every prompt-producing
# runner. No session-global "newest running user message" fallback remains.
# ---------------------------------------------------------------------------
replace_once("src/lib/systemPrompt.ts", "import { useSessionStore } from '../store/sessionStore';\n", "")
replace_once("src/lib/systemPrompt.ts", "import { textContent } from './llamaClient';\n", "")
regex_once(
    "src/lib/systemPrompt.ts",
    r'/\*\* Existing call sites predate a task-text argument\..*?\nfunction runningTaskText\(\): string \{.*?\n\}\n\n',
    '',
    flags=re.S,
)
replace_once(
    "src/lib/systemPrompt.ts",
    "  taskText: string = '',\n  fileHints: string[] = [],\n): string {\n",
    "  taskText: string = '',\n  fileHints: string[] = [],\n  frozenStandardsSection: string | null = null,\n): string {\n",
)
replace_once(
    "src/lib/systemPrompt.ts",
    "  const effectiveTaskText = taskText.trim() || runningTaskText();\n  const applicableStandardsSection = effectiveTaskText\n    ? standardsPromptSection(useStandardsStore.getState().preview(effectiveTaskText, fileHints))\n    : '';\n",
    "  const effectiveTaskText = taskText.trim();\n  const applicableStandardsSection = frozenStandardsSection ?? (effectiveTaskText\n    ? standardsPromptSection(useStandardsStore.getState().preview(effectiveTaskText, fileHints))\n    : '');\n",
)

# agentLoop: two independently resolved turn paths, both freeze once immediately
# after resolving the user's attachments and pass the frozen section thereafter.
replace_once(
    "src/lib/agentLoop.ts",
    "import { currentSystemPrompt, ULTRACODE_SYSTEM_SECTION, type AttachedStackPromptInfo } from './systemPrompt';\n",
    "import { currentSystemPrompt, ULTRACODE_SYSTEM_SECTION, type AttachedStackPromptInfo } from './systemPrompt';\nimport { freezeStandardsTaskContext } from './standardsTaskContext';\n",
)
replace_count(
    "src/lib/agentLoop.ts",
    "  const { textRefs, images, unresolved } = await resolveReferences(userText, attachments);\n",
    "  const { textRefs, images, unresolved } = await resolveReferences(userText, attachments);\n  const frozenStandards = await freezeStandardsTaskContext(userText, textRefs, images);\n",
    2,
)
replace_once(
    "src/lib/agentLoop.ts",
    "      currentSystemPrompt(session?.personaId ?? null, attachedStacksForPrompt, docChatMode),\n",
    "      currentSystemPrompt(session?.personaId ?? null, attachedStacksForPrompt, docChatMode, frozenStandards.taskText, frozenStandards.fileHints, frozenStandards.section),\n",
)
replace_once(
    "src/lib/agentLoop.ts",
    "          currentSystemPrompt(personaId, attachedStacksForPrompt, docChatMode),\n",
    "          currentSystemPrompt(personaId, attachedStacksForPrompt, docChatMode, frozenStandards.taskText, frozenStandards.fileHints, frozenStandards.section),\n",
)

# Mechanical checks for applicable frozen standards participate in the ordinary
# post-mutation Verification phase even if global auto-verify is disabled.
replace_once(
    "src/lib/agentLoop.ts",
    "  signal?: AbortSignal\n): Promise<VerifyFailure | null> {\n  if (!useSettingsStore.getState().verifyEnabled) return null;\n",
    "  signal?: AbortSignal,\n  requiredCommandIds: readonly string[] = [],\n): Promise<VerifyFailure | null> {\n  const autoVerifyEnabled = useSettingsStore.getState().verifyEnabled;\n  if (!autoVerifyEnabled && requiredCommandIds.length === 0) return null;\n",
)
replace_once(
    "src/lib/agentLoop.ts",
    "  const enabledCommands = config.commands.filter((c) => c.enabled);\n",
    "  const required = new Set(requiredCommandIds);\n  const enabledCommands = config.commands.filter((c) => c.enabled && (autoVerifyEnabled || required.has(c.id)));\n  const unavailableRequired = requiredCommandIds.filter((id) => !enabledCommands.some((command) => command.id === id));\n  if (unavailableRequired.length > 0) {\n    return { label: 'Standards verification binding', code: null, output: `Required Verification command is disabled or missing: ${unavailableRequired.join(', ')}` };\n  }\n",
)
replace_once(
    "src/lib/agentLoop.ts",
    "        const failure = await runVerificationPhase(sessionId, turnId, addMessage, signal);\n",
    "        const failure = await runVerificationPhase(sessionId, turnId, addMessage, signal, frozenStandards.checkerCommandIds);\n",
)
replace_once(
    "src/lib/agentLoop.ts",
    "        if (settings.verifyEnabled && !signal?.aborted) {\n",
    "        if ((settings.verifyEnabled || frozenStandards.checkerCommandIds.length > 0) && !signal?.aborted) {\n",
)

# Crew and Compare freeze after their one attachment-resolution boundary.
for path in ("src/lib/crewRunner.ts", "src/lib/compareRunner.ts"):
    replace_once(path, 'import { currentSystemPrompt } from "./systemPrompt";\n', 'import { currentSystemPrompt } from "./systemPrompt";\nimport { freezeStandardsTaskContext } from "./standardsTaskContext";\n')
    replace_once(
        path,
        '  const { textRefs, images, unresolved } = await resolveReferences(normalizedPrompt, [...attachments]);\n',
        '  const { textRefs, images, unresolved } = await resolveReferences(normalizedPrompt, [...attachments]);\n  const frozenStandards = await freezeStandardsTaskContext(normalizedPrompt, textRefs, images);\n',
    )

replace_once(
    "src/lib/crewRunner.ts",
    'currentSystemPrompt(source.personaId, attachedStackPromptInfo(stacks), source.docChatMode)',
    'currentSystemPrompt(source.personaId, attachedStackPromptInfo(stacks), source.docChatMode, frozenStandards.taskText, frozenStandards.fileHints, frozenStandards.section)',
)
replace_once(
    "src/lib/compareRunner.ts",
    'currentSystemPrompt(source.personaId, attachedStackPromptInfo(stacks), source.docChatMode)',
    'currentSystemPrompt(source.personaId, attachedStackPromptInfo(stacks), source.docChatMode, frozenStandards.taskText, frozenStandards.fileHints, frozenStandards.section)',
)

# Compare Lab freezes all suite prompts before any branch fan-out so a Settings
# edit mid-run cannot make two branches of one benchmark use different policy.
replace_once(
    "src/lib/compareLabRunner.ts",
    'import { currentSystemPrompt } from "./systemPrompt";\n',
    'import { currentSystemPrompt } from "./systemPrompt";\nimport { freezeStandardsTaskContext, type FrozenStandardsTaskContext } from "./standardsTaskContext";\n',
)
replace_once(
    "src/lib/compareLabRunner.ts",
    'function labSystemPrompt(toolsEnabled: boolean): string {\n  const base = currentSystemPrompt(null, [], false);\n',
    'function labSystemPrompt(toolsEnabled: boolean, standards: FrozenStandardsTaskContext): string {\n  const base = currentSystemPrompt(null, [], false, standards.taskText, standards.fileHints, standards.section);\n',
)
replace_once(
    "src/lib/compareLabRunner.ts",
    '  signal: AbortSignal,\n): Promise<void> {\n',
    '  signal: AbortSignal,\n  frozenStandards: FrozenStandardsTaskContext,\n): Promise<void> {\n',
)
replace_once(
    "src/lib/compareLabRunner.ts",
    '    { role: "system", content: labSystemPrompt(toolsOffered) },\n',
    '    { role: "system", content: labSystemPrompt(toolsOffered, frozenStandards) },\n',
)
replace_once(
    "src/lib/compareLabRunner.ts",
    '    try {\n      const remoteWork = run.prompts.flatMap((prompt) =>\n        remoteTargets.map((target) => runLabPair(run.id, prompt, target, costRates[target.key], controller.signal)),\n      );\n',
    '    try {\n      const standardsByPrompt = new Map<string, FrozenStandardsTaskContext>();\n      for (const prompt of run.prompts) standardsByPrompt.set(prompt.id, await freezeStandardsTaskContext(prompt.text));\n      const remoteWork = run.prompts.flatMap((prompt) =>\n        remoteTargets.map((target) => runLabPair(run.id, prompt, target, costRates[target.key], controller.signal, standardsByPrompt.get(prompt.id)!)),\n      );\n',
)
replace_once(
    "src/lib/compareLabRunner.ts",
    '          await runLabPair(run.id, prompt, target, costRates[target.key], controller.signal);\n',
    '          await runLabPair(run.id, prompt, target, costRates[target.key], controller.signal, standardsByPrompt.get(prompt.id)!);\n',
)

# Remove legacy checker fields from the focused branch tests and add regression
# assertions for authority stripping/conflict recovery.
test_path = "src/lib/standards.test.ts"
test_text = read(test_path)
test_text = re.sub(r'\n\s*checker_command_ids:\s*\[\],', '', test_text)
write(test_path, test_text)

write(
    "src/lib/standardsSecurity.test.ts",
    textwrap.dedent('''\
    import { describe, expect, it } from "vitest";
    import { detectStandardConflicts, validateStandardsDocument, type EngineeringStandard } from "./standards";

    function standard(id: string, status: EngineeringStandard["status"], approvedAt: number | null): EngineeringStandard {
      return {
        standard_id: id,
        version: 1,
        title: id,
        body: `policy ${id}`,
        scope: "repository",
        scope_path: null,
        applicability: { globs: [], languages: [], frameworks: [], task_keywords: [] },
        severity: "recommended",
        status,
        origin: "manual",
        confidence: 1,
        tags: [],
        evidence: [],
        conflicts_with: [],
        supersedes: null,
        created_at_ms: 1,
        approved_at_ms: approvedAt,
        last_verified_at_ms: 1,
        content_sha256: "a".repeat(64),
        drift: "healthy",
        revision_history: [],
        pending_revision: null,
      };
    }

    describe("Standards Studio authority boundary", () => {
      it("strips legacy checker ids from portable schema-v1 data", () => {
        const value = {
          schema_version: 1,
          workspace_id: "/repo",
          generated_at_ms: 1,
          standards: [{ ...standard("one", "candidate", null), checker_command_ids: ["run-anything"] }],
        };
        const parsed = validateStandardsDocument(value);
        expect("checker_command_ids" in parsed.standards[0]).toBe(false);
      });

      it("restores approved/candidate lifecycle when a derived conflict disappears", () => {
        const approved = { ...standard("approved", "conflicting", 10), conflicts_with: ["candidate"] };
        const candidate = standard("candidate", "rejected", null);
        const resolved = detectStandardConflicts([approved, candidate]);
        expect(resolved[0].status).toBe("approved");
      });
    });
    '''),
)

# Documentation must describe the real post-finalization authority boundary.
doc = read("docs/standards-studio.md")
doc += textwrap.dedent('''\

## Local mechanical checker authority

Verification bindings are app-local workspace state, not part of `.little-monkey/standards/index.json` or portable imports. A cloned or imported repository can therefore propose policy but cannot grant process execution. The app accepts only IDs of currently enabled Verification commands, re-resolves them immediately before execution, and fails closed when a binding was disabled or removed. Applicable bound checks join the normal post-mutation verification phase.

## Frozen task selection

Normal turns, resident turns, Crew, Compare, and Compare Lab freeze the exact user task, referenced file/image paths, selected standards, selection provenance, and applicable checker IDs before model fan-out. `currentSystemPrompt` has no global-running-session fallback, so concurrent panes cannot leak another pane's task into Standards selection and a Standards Studio edit cannot change one operation halfway through its run.
''')
write("docs/standards-studio.md", doc)

print("Standards Studio finalizer applied successfully")
