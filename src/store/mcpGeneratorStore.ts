import { create } from "zustand";
import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";

import {
  generateMcpServerCode,
  resolveGeneratorTarget,
  suggestedFileName,
  validateServerSpec,
  type McpServerSpec,
  type McpSourceKind,
  type McpToolParamSpec,
  type McpToolSpec,
} from "../lib/mcpGenerator";
import { runSimulation, type SimulationReport } from "../lib/mcpSimulator";

const STORAGE_KEY = "little-monkey-mcp-generator-v1";

export interface GeneratedMcpServerEntry {
  id: string;
  spec: McpServerSpec;
  code: string | null;
  simulation: SimulationReport | null;
  /** True only once a simulation has run AND come back clean — the
   * acceptance gate ("Generated MCP servers must pass the simulator before
   * install"). Recomputed on every generate/simulate, never set directly. */
  ready: boolean;
  savedPath: string | null;
  createdAt: number;
  updatedAt: number;
}

export function emptyToolDraft(): McpToolSpec {
  return { name: "", description: "", requiresAuth: false, params: [] };
}

export function emptyParamDraft(): McpToolParamSpec {
  return { name: "", type: "string", required: true, description: "" };
}

export function emptyServerDraft(): McpServerSpec {
  return { name: "", description: "", sourceKind: "api", target: "", tools: [emptyToolDraft()] };
}

interface McpGeneratorStore {
  draft: McpServerSpec;
  entries: GeneratedMcpServerEntry[];
  selectedEntryId: string | null;
  generating: boolean;
  simulating: boolean;
  saving: boolean;
  error: string | null;

  updateDraft: (patch: Partial<McpServerSpec>) => void;
  resetDraft: () => void;
  addTool: () => void;
  removeTool: (toolIndex: number) => void;
  updateTool: (toolIndex: number, patch: Partial<McpToolSpec>) => void;
  addParam: (toolIndex: number) => void;
  removeParam: (toolIndex: number, paramIndex: number) => void;
  updateParam: (toolIndex: number, paramIndex: number, patch: Partial<McpToolParamSpec>) => void;

  generate: () => Promise<GeneratedMcpServerEntry>;
  runSimulator: (entryId: string) => void;
  selectEntry: (entryId: string | null) => void;
  removeEntry: (entryId: string) => void;
  saveToDisk: (entryId: string) => Promise<string | null>;
}

function persist(entries: GeneratedMcpServerEntry[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ version: 1, entries }));
  } catch {
    // Non-fatal: the entry stays live in memory for the rest of this session.
  }
}

function hydrate(): GeneratedMcpServerEntry[] {
  try {
    const raw = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null") as { version?: unknown; entries?: unknown } | null;
    if (raw?.version !== 1 || !Array.isArray(raw.entries)) return [];
    return raw.entries.filter((value): value is GeneratedMcpServerEntry => {
      const entry = value as Partial<GeneratedMcpServerEntry>;
      return Boolean(
        entry &&
        typeof entry.id === "string" &&
        entry.spec &&
        typeof entry.spec === "object" &&
        typeof entry.createdAt === "number",
      );
    });
  } catch {
    return [];
  }
}

export const useMcpGeneratorStore = create<McpGeneratorStore>((set, get) => ({
  draft: emptyServerDraft(),
  entries: hydrate(),
  selectedEntryId: null,
  generating: false,
  simulating: false,
  saving: false,
  error: null,

  updateDraft: (patch) => set((state) => ({ draft: { ...state.draft, ...patch } })),

  resetDraft: () => set({ draft: emptyServerDraft(), error: null }),

  addTool: () => set((state) => ({ draft: { ...state.draft, tools: [...state.draft.tools, emptyToolDraft()] } })),

  removeTool: (toolIndex) =>
    set((state) => ({ draft: { ...state.draft, tools: state.draft.tools.filter((_, i) => i !== toolIndex) } })),

  updateTool: (toolIndex, patch) =>
    set((state) => ({
      draft: {
        ...state.draft,
        tools: state.draft.tools.map((tool, i) => (i === toolIndex ? { ...tool, ...patch } : tool)),
      },
    })),

  addParam: (toolIndex) =>
    set((state) => ({
      draft: {
        ...state.draft,
        tools: state.draft.tools.map((tool, i) =>
          i === toolIndex ? { ...tool, params: [...tool.params, emptyParamDraft()] } : tool,
        ),
      },
    })),

  removeParam: (toolIndex, paramIndex) =>
    set((state) => ({
      draft: {
        ...state.draft,
        tools: state.draft.tools.map((tool, i) =>
          i === toolIndex ? { ...tool, params: tool.params.filter((_, p) => p !== paramIndex) } : tool,
        ),
      },
    })),

  updateParam: (toolIndex, paramIndex, patch) =>
    set((state) => ({
      draft: {
        ...state.draft,
        tools: state.draft.tools.map((tool, i) =>
          i === toolIndex
            ? { ...tool, params: tool.params.map((param, p) => (p === paramIndex ? { ...param, ...patch } : param)) }
            : tool,
        ),
      },
    })),

  generate: async () => {
    const spec = get().draft;
    const issues = validateServerSpec(spec);
    if (issues.length > 0) {
      const message = issues.join("\n");
      set({ error: message });
      throw new Error(message);
    }
    set({ generating: true, error: null });
    try {
      const target = await resolveGeneratorTarget();
      const code = await generateMcpServerCode(spec, target);
      const now = Date.now();
      const entry: GeneratedMcpServerEntry = {
        id: crypto.randomUUID(),
        spec: structuredClone(spec),
        code,
        simulation: null,
        ready: false,
        savedPath: null,
        createdAt: now,
        updatedAt: now,
      };
      const entries = [entry, ...get().entries];
      persist(entries);
      set({ entries, selectedEntryId: entry.id });
      return entry;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      set({ error: message });
      throw error;
    } finally {
      set({ generating: false });
    }
  },

  runSimulator: (entryId) => {
    const entry = get().entries.find((candidate) => candidate.id === entryId);
    if (!entry) return;
    set({ simulating: true, error: null });
    try {
      const report = runSimulation(entry.spec);
      const entries = get().entries.map((candidate) =>
        candidate.id === entryId
          ? { ...candidate, simulation: report, ready: report.clean, updatedAt: Date.now() }
          : candidate,
      );
      persist(entries);
      set({ entries });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      set({ error: message });
    } finally {
      set({ simulating: false });
    }
  },

  selectEntry: (entryId) => set({ selectedEntryId: entryId }),

  removeEntry: (entryId) => {
    const entries = get().entries.filter((entry) => entry.id !== entryId);
    persist(entries);
    set({
      entries,
      selectedEntryId: get().selectedEntryId === entryId ? null : get().selectedEntryId,
    });
  },

  saveToDisk: async (entryId) => {
    const entry = get().entries.find((candidate) => candidate.id === entryId);
    if (!entry || !entry.code) throw new Error("Generate the server code before saving it.");
    if (!entry.ready) {
      throw new Error("This server has not passed the simulator yet — run the simulator and resolve every failure before saving it for install.");
    }
    set({ saving: true, error: null });
    try {
      const destination = await save({
        defaultPath: suggestedFileName(entry.spec),
        filters: [{ name: "TypeScript", extensions: ["ts"] }],
      });
      if (!destination) return null;
      await writeTextFile(destination, entry.code);
      const entries = get().entries.map((candidate) =>
        candidate.id === entryId ? { ...candidate, savedPath: destination, updatedAt: Date.now() } : candidate,
      );
      persist(entries);
      set({ entries });
      return destination;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      set({ error: message });
      throw error;
    } finally {
      set({ saving: false });
    }
  },
}));

export type { McpServerSpec, McpSourceKind, McpToolParamSpec, McpToolSpec };
