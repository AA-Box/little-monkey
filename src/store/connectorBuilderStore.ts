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
 * 4. Registration stays blocked because this client currently has no
 *    generated REST-to-MCP bridge artifact. A clean schema simulation is
 *    useful evidence, but registering the OpenAPI base URL as though it were
 *    an MCP endpoint would create a server that can never connect.
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

/** Mirrors `sopCompilerStore.ts`'s own `MAX_IMPORT_FILE_BYTES` guard — an
 * OpenAPI document has no business being larger than this locally. */
const MAX_IMPORT_FILE_BYTES = 5 * 1024 * 1024;
export const CONNECTOR_BRIDGE_REQUIRED =
  "Registration is blocked: this OpenAPI document describes a REST API, not an MCP server. Generate and execute-verify a REST-to-MCP bridge artifact before making it available to agents.";

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
  /** Availability gate. It remains false until an executable bridge artifact
   * exists and passes an artifact-level probe; spec simulation alone cannot
   * satisfy it. */
  ready: boolean;
  availabilityBlockReason: string | null;

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
  availabilityBlockReason: null,

  registering: false,
  registeredServerId: null,

  error: null,

  setSpecText: (text) => set({
    specText: text,
    specFileName: null,
    definition: null,
    summary: null,
    simulation: null,
    ready: false,
    availabilityBlockReason: null,
    registeredServerId: null,
    error: null,
  }),

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
      set({
        specText: content,
        specFileName: fileName,
        importing: false,
        definition: null,
        summary: null,
        simulation: null,
        ready: false,
        availabilityBlockReason: null,
        registeredServerId: null,
      });
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
      availabilityBlockReason: null,
      registeredServerId: null,
    });
    try {
      const parsed = parseOpenApiSpec(specText, specFileName ?? undefined);
      const definition = buildConnectorDefinition(parsed);
      set({ definition, generating: false, availabilityBlockReason: CONNECTOR_BRIDGE_REQUIRED });
    } catch (error) {
      set({ generating: false, error: errorText(error) });
      return;
    }

    // Best-effort: a failed/timed-out draft never blocks the deterministic
    // definition or schema simulation. Availability is gated separately.
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
      // This validates only the declared schema. There is no executable MCP
      // bridge artifact yet, so it must never flip availability to ready.
      set({ simulation: report, ready: false, availabilityBlockReason: CONNECTOR_BRIDGE_REQUIRED, simulating: false });
    } catch (error) {
      set({ simulating: false, error: errorText(error) });
    }
  },

  registerWithMcp: async () => {
    const { definition } = get();
    if (!definition) throw new Error("Generate a connector before registering it.");
    set({ registering: false, registeredServerId: null, error: CONNECTOR_BRIDGE_REQUIRED });
    throw new Error(CONNECTOR_BRIDGE_REQUIRED);
  },

  reset: () =>
    set({
      specText: "",
      specFileName: null,
      definition: null,
      summary: null,
      simulation: null,
      ready: false,
      availabilityBlockReason: null,
      registeredServerId: null,
      error: null,
    }),
}));
