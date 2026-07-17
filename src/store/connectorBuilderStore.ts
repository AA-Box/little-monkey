/**
 * Connector Builder Studio (ROADMAP.md Phase 7, item 21). Drives the whole
 * "OpenAPI spec -> generated connector -> sandboxed simulation -> register
 * with MCP" flow the panel exposes, entirely client-side:
 *
 * 1. `importFromFile`/`setSpecText` load an OpenAPI document (JSON or YAML,
 *    opened locally via `@tauri-apps/plugin-dialog`, same `open()` +
 *    `stat()` size-guard + `readTextFile()` flow `sopCompilerStore.ts` uses
 *    for its own local-file import).
 * 2. `generate` parses it (`connectorBuilder.ts`, deterministic) into a
 *    `ConnectorDefinition`, then best-effort drafts a human-readable summary
 *    via the same one-shot local-model-call pattern `mcpGenerator.ts` and
 *    `sopCompilerStore.ts` both already use — a summary-draft failure is
 *    logged into `error` but never blocks the definition itself.
 * 3. `runSimulator` runs the SAME `runSimulation` this repo's sibling MCP
 *    Generator/Simulator feature (PR #50) already ships, against the
 *    definition's `McpServerSpec` — reused unchanged, not duplicated. This
 *    is the acceptance gate: `ready` only ever becomes true when
 *    `simulation.clean` is true.
 * 4. `registerWithMcp`, enabled only once `ready`, calls the EXISTING
 *    `useMcpStore().addServer` — the same registration path Settings' own
 *    MCP server list uses — to add a real `McpServerEntry` to
 *    `mcp_servers.json`. See `connectorBuilder.ts`'s module doc comment for
 *    the labeled scope cut: registration adds the config entry; actually
 *    *connecting* it still requires the target URL to speak MCP itself,
 *    which a bare REST API described by an OpenAPI doc does not (a
 *    REST-to-MCP bridge is future work, not this MVP).
 */
import { create } from "zustand";
import { open } from "@tauri-apps/plugin-dialog";
import { readTextFile, stat } from "@tauri-apps/plugin-fs";

import {
  buildConnectorDefinition,
  draftConnectorSummary,
  parseOpenApiSpec,
  resolveConnectorDraftTarget,
  type ConnectorDefinition,
} from "../lib/connectorBuilder";
import { runSimulation, type SimulationReport } from "../lib/mcpSimulator";
import { useMcpStore, type McpServerEntry } from "./mcpStore";

/** Mirrors `sopCompilerStore.ts`'s own `MAX_IMPORT_FILE_BYTES` guard — an
 * OpenAPI document has no business being larger than this locally. */
const MAX_IMPORT_FILE_BYTES = 5 * 1024 * 1024;

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

interface ConnectorBuilderStore {
  specText: string;
  specFileName: string | null;
  importing: boolean;

  definition: ConnectorDefinition | null;
  summary: string | null;
  generating: boolean;
  drafting: boolean;

  simulation: SimulationReport | null;
  simulating: boolean;
  /** The acceptance gate: true only once a simulation has run AND come back
   * clean. Recomputed on every `generate`/`runSimulator` call, never set
   * directly. */
  ready: boolean;

  registering: boolean;
  registeredServerId: string | null;

  error: string | null;

  setSpecText: (text: string) => void;
  importFromFile: () => Promise<void>;
  generate: () => Promise<void>;
  runSimulator: () => void;
  registerWithMcp: () => Promise<string>;
  reset: () => void;
}

export const useConnectorBuilderStore = create<ConnectorBuilderStore>((set, get) => ({
  specText: "",
  specFileName: null,
  importing: false,

  definition: null,
  summary: null,
  generating: false,
  drafting: false,

  simulation: null,
  simulating: false,
  ready: false,

  registering: false,
  registeredServerId: null,

  error: null,

  setSpecText: (text) => set({ specText: text, specFileName: null, error: null }),

  importFromFile: async () => {
    set({ importing: true, error: null });
    try {
      const selected = await open({
        title: "Import an OpenAPI spec",
        multiple: false,
        directory: false,
        filters: [{ name: "OpenAPI spec", extensions: ["json", "yaml", "yml"] }],
      });
      if (typeof selected !== "string") {
        set({ importing: false });
        return;
      }
      const fileInfo = await stat(selected);
      if (fileInfo.size > MAX_IMPORT_FILE_BYTES) {
        throw new Error(`That file is larger than ${Math.floor(MAX_IMPORT_FILE_BYTES / (1024 * 1024))}MB.`);
      }
      const content = await readTextFile(selected);
      const fileName = selected.split(/[\\/]/).pop() ?? selected;
      set({ specText: content, specFileName: fileName, importing: false });
    } catch (error) {
      set({ importing: false, error: errorText(error) });
    }
  },

  generate: async () => {
    const { specText, specFileName } = get();
    if (!specText.trim()) {
      set({ error: "Load or paste an OpenAPI spec before generating a connector." });
      return;
    }
    set({
      generating: true,
      error: null,
      definition: null,
      summary: null,
      simulation: null,
      ready: false,
      registeredServerId: null,
    });
    try {
      const parsed = parseOpenApiSpec(specText, specFileName ?? undefined);
      const definition = buildConnectorDefinition(parsed);
      set({ definition, generating: false });
    } catch (error) {
      set({ generating: false, error: errorText(error) });
      return;
    }

    // Best-effort: a failed/timed-out draft never blocks the (already-set)
    // deterministic definition, simulation, or registration.
    set({ drafting: true });
    try {
      const target = await resolveConnectorDraftTarget();
      const definition = get().definition;
      if (!definition) return;
      const summary = await draftConnectorSummary(definition, target);
      set({ summary, drafting: false });
    } catch (error) {
      set({ drafting: false, error: `Connector generated, but the summary draft failed: ${errorText(error)}` });
    }
  },

  runSimulator: () => {
    const { definition } = get();
    if (!definition) return;
    set({ simulating: true, error: null });
    try {
      const report = runSimulation(definition.server);
      set({ simulation: report, ready: report.clean, simulating: false });
    } catch (error) {
      set({ simulating: false, error: errorText(error) });
    }
  },

  registerWithMcp: async () => {
    const { definition, ready } = get();
    if (!definition) throw new Error("Generate a connector before registering it.");
    if (!ready) {
      throw new Error("This connector has not passed the simulator yet — run the simulator and resolve every failure before it becomes available to agents.");
    }
    set({ registering: true, error: null });
    try {
      const existingIds = new Set(useMcpStore.getState().servers.map((server) => server.id));
      let id = definition.server.name;
      let suffix = 2;
      while (existingIds.has(id)) {
        id = `${definition.server.name}-${suffix}`;
        suffix += 1;
      }
      const entry: McpServerEntry = {
        id,
        label: definition.server.name,
        transport: { type: "http", url: definition.server.target },
        enabled: true,
        tool_allowlist: null,
        timeout_secs: null,
      };
      await useMcpStore.getState().addServer(entry);
      set({ registering: false, registeredServerId: id });
      return id;
    } catch (error) {
      set({ registering: false, error: errorText(error) });
      throw error;
    }
  },

  reset: () =>
    set({
      specText: "",
      specFileName: null,
      definition: null,
      summary: null,
      simulation: null,
      ready: false,
      registeredServerId: null,
      error: null,
    }),
}));
