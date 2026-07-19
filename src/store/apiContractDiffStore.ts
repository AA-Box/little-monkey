/**
 * API Contract Diff and Mock Lab (ROADMAP Phase 7, item 23): loads two local
 * OpenAPI JSON/YAML files (old + new version) via the existing file-open
 * dialog, structurally diffs them, and — for whatever breaking changes come
 * out of that diff — drives one batched one-shot local-model call to draft a
 * plain-English client-impact note plus a migration suggestion for each,
 * exactly the way `sopCompilerStore.ts` drives its own one-shot compiler
 * call around `agentLoop.ts`'s `resolveTarget` + `turnEngine.ts`'s
 * `attemptStream`. No new Rust command and no persistence — this is a
 * per-session working tool, not a durable artifact store.
 */
import { create } from "zustand";
import { open } from "@tauri-apps/plugin-dialog";
import { readTextFile, stat } from "@tauri-apps/plugin-fs";

import { resolveTarget } from "../lib/agentLoop";
import { attemptStream } from "../lib/turnEngine";
import type { ChatMessage } from "../lib/llamaClient";
import { effortForTarget } from "./modelStore";
import {
  diffApiDocuments,
  draftClientImpactNotes,
  generateContractTestStub,
  generateMockResponses,
  parseOpenApiDocument,
  type ApiChange,
  type ApiContractDiffCallResult,
  type ApiDocument,
  type ClientImpactNote,
  type MockExample,
} from "../lib/apiContractDiff";

/** Fixed pseudo-session id for this lab's one-shot model call — this run
 * never belongs to a chat session; `recordUsage: false` (see `compile`-style
 * callers in `sopCompilerStore.ts`) means it never writes into
 * `useUsageStore` under it, so this only needs to be a stable, non-empty
 * string. */
const API_CONTRACT_DIFF_SESSION_ID = "api-contract-diff-lab";
/** Reject an imported file above this size outright — mirrors
 * `sopCompilerStore.ts`'s own `stat().size` guard on file import. */
const MAX_IMPORT_FILE_BYTES = 5 * 1024 * 1024;
const SPEC_DIALOG_FILTERS = [{ name: "OpenAPI spec", extensions: ["json", "yaml", "yml"] }];

export type DiffSlot = "old" | "new";

interface LoadedSpec {
  fileName: string;
  text: string;
  doc: ApiDocument;
}

interface ApiContractDiffStore {
  oldSpec: LoadedSpec | null;
  newSpec: LoadedSpec | null;
  loadingSlot: DiffSlot | null;
  loadError: string | null;

  changes: ApiChange[];
  mocks: MockExample[];
  testStub: string;
  /** True once `runDiff` has completed at least once against the currently
   * loaded pair — distinguishes "diffed, zero changes found" (a real,
   * displayable release-ready verdict) from "not diffed yet" (no verdict to
   * show at all). Reset to `false` whenever either file slot is reloaded. */
  hasRun: boolean;
  diffError: string | null;

  drafting: boolean;
  draftError: string | null;
  impactNotes: ClientImpactNote[];

  loadFile: (slot: DiffSlot) => Promise<void>;
  runDiff: () => void;
  draftImpactNotes: () => Promise<void>;
  reset: () => void;
}

async function loadSpecFromDialog(slot: DiffSlot): Promise<LoadedSpec | null> {
  const selected = await open({
    title: slot === "old" ? "Open the OLD OpenAPI spec" : "Open the NEW OpenAPI spec",
    multiple: false,
    directory: false,
    filters: SPEC_DIALOG_FILTERS,
  });
  if (typeof selected !== "string") return null;
  const fileInfo = await stat(selected);
  if (fileInfo.size > MAX_IMPORT_FILE_BYTES) {
    throw new Error(`That file is larger than ${Math.floor(MAX_IMPORT_FILE_BYTES / (1024 * 1024))}MB.`);
  }
  const text = await readTextFile(selected);
  const fileName = selected.split(/[\\/]/).pop() ?? selected;
  const doc = parseOpenApiDocument(text, fileName);
  return { fileName, text, doc };
}

export const useApiContractDiffStore = create<ApiContractDiffStore>((set, get) => ({
  oldSpec: null,
  newSpec: null,
  loadingSlot: null,
  loadError: null,

  changes: [],
  mocks: [],
  testStub: "",
  hasRun: false,
  diffError: null,

  drafting: false,
  draftError: null,
  impactNotes: [],

  loadFile: async (slot) => {
    set({ loadingSlot: slot, loadError: null });
    try {
      const spec = await loadSpecFromDialog(slot);
      if (!spec) {
        set({ loadingSlot: null });
        return;
      }
      // Loading a new file invalidates whatever diff/mocks/notes were
      // computed against the previous pairing — never leave a stale report
      // on screen next to a freshly swapped-in spec.
      const slotUpdate: Partial<ApiContractDiffStore> = slot === "old" ? { oldSpec: spec } : { newSpec: spec };
      set({
        ...slotUpdate,
        loadingSlot: null,
        changes: [],
        mocks: [],
        testStub: "",
        hasRun: false,
        diffError: null,
        impactNotes: [],
        draftError: null,
      });
    } catch (err) {
      set({ loadingSlot: null, loadError: err instanceof Error ? err.message : String(err) });
    }
  },

  runDiff: () => {
    const { oldSpec, newSpec } = get();
    if (!oldSpec || !newSpec) {
      set({ diffError: "Load both an old and a new OpenAPI spec before running the diff." });
      return;
    }
    try {
      const changes = diffApiDocuments(oldSpec.doc, newSpec.doc);
      const mocks = generateMockResponses(newSpec.doc);
      const testStub = generateContractTestStub(newSpec.doc);
      set({ changes, mocks, testStub, hasRun: true, diffError: null, impactNotes: [], draftError: null });
    } catch (err) {
      set({ hasRun: false, diffError: err instanceof Error ? err.message : String(err) });
    }
  },

  draftImpactNotes: async () => {
    const { changes } = get();
    const breaking = changes.filter((change) => change.severity === "breaking");
    if (breaking.length === 0) {
      set({ draftError: "There are no breaking changes to draft client-impact notes for." });
      return;
    }
    set({ drafting: true, draftError: null });
    try {
      const target = await resolveTarget();
      const callModel = async (messages: ChatMessage[], signal?: AbortSignal): Promise<ApiContractDiffCallResult> => {
        const result = await attemptStream(
          target,
          messages,
          [],
          signal,
          effortForTarget(target),
          API_CONTRACT_DIFF_SESSION_ID,
          undefined,
          false,
        );
        return { content: result.content, streamError: result.streamError };
      };
      const notes = await draftClientImpactNotes(changes, callModel);
      set({ impactNotes: notes, drafting: false });
    } catch (err) {
      set({ drafting: false, draftError: err instanceof Error ? err.message : String(err) });
    }
  },

  reset: () =>
    set({
      oldSpec: null,
      newSpec: null,
      loadingSlot: null,
      loadError: null,
      changes: [],
      mocks: [],
      testStub: "",
      hasRun: false,
      diffError: null,
      drafting: false,
      draftError: null,
      impactNotes: [],
    }),
}));
