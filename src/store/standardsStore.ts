import { invoke } from "@tauri-apps/api/core";
import { create } from "zustand";

import { primaryRoot, useWorkspaceStore } from "./workspaceStore";
import { useVerifyStore } from "./verifyStore";
import {
  DEFAULT_STANDARDS_CHAR_BUDGET,
  emptyStandardsDocument,
  selectStandards,
  snapshotStandardRevision,
  validateStandardsDocument,
  type EngineeringStandard,
  type StandardsDocument,
  type StandardsSelection,
} from "../lib/standards";
import {
  approveStandard,
  discoverAndMergeStandards,
  exportAgentOsStandards,
  exportStandards,
  importAgentOsStandards,
  loadStandards,
  saveStandards,
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

function stripPortableCheckerAuthority(document: StandardsDocument): StandardsDocument {
  return {
    ...document,
    standards: document.standards.map((standard) => ({ ...standard, checker_command_ids: [] })),
  };
}

function resolveConflictLifecycle(standards: EngineeringStandard[]): EngineeringStandard[] {
  const activeIds = new Set(
    standards
      .filter((standard) => standard.status === "approved" || standard.status === "candidate" || standard.status === "conflicting")
      .map((standard) => standard.standard_id),
  );
  const conflicting = new Set<string>();
  for (const standard of standards) {
    if (!activeIds.has(standard.standard_id)) continue;
    for (const other of standard.conflicts_with) {
      if (!activeIds.has(other)) continue;
      conflicting.add(standard.standard_id);
      conflicting.add(other);
    }
  }
  return standards.map((standard) => {
    if (["rejected", "deprecated", "stale"].includes(standard.status)) return standard;
    if (conflicting.has(standard.standard_id)) return { ...standard, status: "conflicting" as const };
    if (standard.status === "conflicting") {
      return { ...standard, status: standard.approved_at_ms === null ? "candidate" as const : "approved" as const };
    }
    return standard;
  });
}

async function sha256Text(value: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function policyDigest(standard: Pick<EngineeringStandard, "title" | "body" | "applicability" | "severity" | "tags">): Promise<string> {
  return sha256Text(JSON.stringify({
    title: standard.title,
    body: standard.body,
    applicability: standard.applicability,
    severity: standard.severity,
    tags: standard.tags,
  }));
}

async function evaluateDrift(document: StandardsDocument): Promise<StandardsDocument> {
  const now = Date.now();
  const standards: EngineeringStandard[] = [];
  for (const standard of document.standards) {
    const supports = standard.evidence.filter((entry) => entry.supports);
    const shouldEvaluate = supports.length > 0 && ["approved", "stale", "conflicting"].includes(standard.status);
    if (!shouldEvaluate) {
      standards.push(standard.pending_revision && standard.drift === "healthy" ? { ...standard, drift: "weakened" } : standard);
      continue;
    }
    let unchanged = 0;
    let changed = 0;
    let missing = 0;
    for (const evidence of supports) {
      let current: string | null = null;
      try {
        current = await invoke<string>("tool_read_file", { path: evidence.path, workspace_root_override: null });
      } catch {
        current = null;
      }
      if (current === null) {
        missing += 1;
        continue;
      }
      if ((await sha256Text(current)) === evidence.sha256) unchanged += 1;
      else changed += 1;
    }
    let drift: EngineeringStandard["drift"] = unchanged > 0 && changed === 0 && missing === 0
      ? "healthy"
      : unchanged === 0 && (changed > 0 || missing > 0)
        ? "contradicted"
        : "weakened";
    if (standard.pending_revision && drift === "healthy") drift = "weakened";
    let status = standard.status;
    if (drift === "contradicted" && status === "approved") status = "stale";
    else if (status === "stale" && drift !== "contradicted") status = "approved";
    standards.push({ ...standard, drift, status, last_verified_at_ms: now });
  }
  return { ...document, standards: resolveConflictLifecycle(standards), generated_at_ms: now };
}

async function secureImport(workspacePath: string): Promise<StandardsDocument> {
  const raw = await invoke<string>("tool_read_file", {
    path: STANDARDS_IMPORT_PATH,
    workspace_root_override: null,
  });
  const incoming = stripPortableCheckerAuthority(validateStandardsDocument(JSON.parse(raw)));
  const current = stripPortableCheckerAuthority(await loadStandards(workspacePath));
  const byId = new Map(current.standards.map((standard) => [standard.standard_id, standard]));

  for (const untrusted of incoming.standards) {
    const digest = await policyDigest(untrusted);
    const imported: EngineeringStandard = {
      ...untrusted,
      origin: "imported",
      status: "candidate",
      approved_at_ms: null,
      content_sha256: digest,
      revision_history: [],
      pending_revision: null,
      checker_command_ids: [],
    };
    const existing = byId.get(imported.standard_id);
    if (!existing) {
      byId.set(imported.standard_id, imported);
      continue;
    }
    if (existing.content_sha256 === digest) {
      byId.set(imported.standard_id, {
        ...existing,
        evidence: imported.evidence.map((entry) => ({ ...entry })),
        last_verified_at_ms: Date.now(),
        checker_command_ids: [],
      });
      continue;
    }
    const locallyApproved = existing.approved_at_ms !== null && existing.status !== "rejected" && existing.status !== "deprecated";
    if (locallyApproved) {
      const now = Date.now();
      byId.set(imported.standard_id, {
        ...existing,
        drift: existing.drift === "contradicted" ? "contradicted" : "weakened",
        checker_command_ids: [],
        pending_revision: {
          version: existing.version + 1,
          title: imported.title,
          body: imported.body,
          applicability: structuredClone(imported.applicability),
          severity: imported.severity,
          tags: [...imported.tags],
          evidence: imported.evidence.map((entry) => ({ ...entry })),
          content_sha256: digest,
          recorded_at_ms: now,
          proposed_at_ms: now,
          source: "imported",
        },
      });
      continue;
    }
    byId.set(imported.standard_id, {
      ...imported,
      version: existing.version + 1,
      created_at_ms: existing.created_at_ms,
      revision_history: [...existing.revision_history, snapshotStandardRevision(existing, "imported_revision")],
    });
  }

  const next = {
    ...current,
    standards: resolveConflictLifecycle([...byId.values()]),
    generated_at_ms: Date.now(),
  };
  await saveStandards(workspacePath, next);
  return next;
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
      const loaded = stripPortableCheckerAuthority(await loadStandards(workspacePath));
      const document = await evaluateDrift(loaded);
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
      const discovered = stripPortableCheckerAuthority(await discoverAndMergeStandards(workspacePath));
      const document = { ...discovered, standards: resolveConflictLifecycle(discovered.standards) };
      await saveStandards(workspacePath, document);
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
    const approved = stripPortableCheckerAuthority(await approveStandard(workspacePath, document, standardId));
    const next = { ...approved, standards: resolveConflictLifecycle(approved.standards) };
    await saveStandards(workspacePath, next);
    set({ document: next, error: null });
  },

  reject: async (standardId) => {
    const workspacePath = activeWorkspace();
    const document = get().document;
    if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
    const rejected = stripPortableCheckerAuthority(await setStandardStatus(workspacePath, document, standardId, "rejected"));
    const next = { ...rejected, standards: resolveConflictLifecycle(rejected.standards) };
    await saveStandards(workspacePath, next);
    set({ document: next, error: null });
  },

  rejectRevision: async (standardId) => {
    const workspacePath = activeWorkspace();
    const document = get().document;
    if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
    const standards = document.standards.map((standard) => standard.standard_id === standardId && standard.pending_revision
      ? { ...standard, pending_revision: null }
      : standard);
    const next = await evaluateDrift({ ...document, standards, generated_at_ms: Date.now() });
    await saveStandards(workspacePath, next);
    set({ document: next, error: null });
  },

  deprecate: async (standardId) => {
    const workspacePath = activeWorkspace();
    const document = get().document;
    if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
    const deprecated = stripPortableCheckerAuthority(await setStandardStatus(workspacePath, document, standardId, "deprecated"));
    const next = { ...deprecated, standards: resolveConflictLifecycle(deprecated.standards) };
    await saveStandards(workspacePath, next);
    set({ document: next, error: null });
  },

  drift: async () => {
    const workspacePath = activeWorkspace();
    const document = get().document;
    if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
    set({ loading: true, error: null });
    try {
      const next = await evaluateDrift(document);
      await saveStandards(workspacePath, next);
      set({ document: next, loading: false });
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
      const document = await secureImport(workspacePath);
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
      const path = await exportStandards(stripPortableCheckerAuthority(document));
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
      const imported = stripPortableCheckerAuthority(await importAgentOsStandards(workspacePath));
      const document = { ...imported, standards: resolveConflictLifecycle(imported.standards) };
      await saveStandards(workspacePath, document);
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
      const path = await exportAgentOsStandards(stripPortableCheckerAuthority(document));
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
    const standard = get().document?.standards.find((entry) => entry.standard_id === standardId);
    if (!standard) throw new Error(`Unknown standard ${standardId}.`);
    const commandIds = get().checkerBindings[standardId] ?? [];
    if (commandIds.length === 0) throw new Error("No Verification commands are bound to this standard.");
    await useVerifyStore.getState().refresh();
    const enabled = new Set(useVerifyStore.getState().config.commands.filter((command) => command.enabled).map((command) => command.id));
    const unavailable = commandIds.filter((id) => !enabled.has(id));
    if (unavailable.length > 0) throw new Error(`Bound Verification command is disabled or missing: ${unavailable.join(", ")}.`);
    const results: VerifyResult[] = [];
    for (const commandId of commandIds) {
      results.push(await invoke<VerifyResult>("verify_run", { commandId, turnId: null, sandboxPath: null }));
    }
    set((state) => ({ lastCheckerResults: { ...state.lastCheckerResults, [standardId]: results }, error: null }));
    return results;
  },

  clearMessage: undefined as never,
}));
