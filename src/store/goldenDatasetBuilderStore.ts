/**
 * Synthetic Data and Golden Dataset Builder (ROADMAP.md Phase 7, item 30):
 * holds named golden datasets built from a seed description/schema, each
 * fully traceable back to its sources (synthetic generation prompt or
 * imported source label), privacy-filter verdicts, duplicate verdicts, and
 * eval results — `../lib/goldenDatasetBuilder.ts` owns the actual
 * generation/dedupe/privacy/eval logic (pure, no React/zustand); this store
 * owns dataset persistence plus the real model round trip.
 *
 * The one-shot generation call is built the exact same way
 * `sopCompilerStore.ts`'s `compile()` builds its own — `agentLoop.ts`'s
 * `resolveTarget()` (no chat session involved; this feature isn't tied to
 * one) + `turnEngine.ts`'s `attemptStream` — rather than the session-aware
 * target resolution `evidenceBoardStore.ts` needs for its session-backed
 * boards.
 *
 * Persisted to localStorage (not zustand's `persist` middleware) with the
 * same hand-rolled versioned-envelope shape `skillProposalStore.ts`/
 * `evidenceBoardStore.ts` use — datasets are lightweight, so a corrupt/
 * incompatible payload is simply dropped on hydrate rather than crashing
 * the app.
 */
import { create } from "zustand";

import { resolveTarget } from "../lib/agentLoop";
import { attemptStream } from "../lib/turnEngine";
import { effortForTarget } from "./modelStore";
import type { ChatMessage } from "../lib/llamaClient";
import {
  generateSyntheticExamples,
  materializeExample,
  newDatasetId,
  newVersionEntry,
  parseFieldsInput,
  parseImportedExamples,
  recomputeDuplicates,
  runSchemaConformanceEval,
  type DatasetExample,
  type EvalRunResult,
  type GoldenDataset,
  type ModelCallResult,
} from "../lib/goldenDatasetBuilder";

const STORAGE_KEY = "little-monkey-golden-datasets-v1";
const STORAGE_VERSION = 1;
/** Fixed pseudo-session id for this feature's one-shot model call — this run
 * never belongs to a chat session, mirrors `sopCompilerStore.ts`'s
 * `SOP_COMPILER_SESSION_ID`. */
const GOLDEN_DATASET_SESSION_ID = "golden-dataset-builder";

function persist(datasets: GoldenDataset[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ version: STORAGE_VERSION, datasets }));
  } catch {
    // Best-effort only — datasets stay live in memory for the rest of this session.
  }
}

function isPrivacyResult(value: unknown): boolean {
  const item = value as { passed?: unknown; findings?: unknown } | null;
  return Boolean(item && typeof item.passed === "boolean" && Array.isArray(item.findings));
}

function isProvenance(value: unknown): boolean {
  const item = value as { kind?: unknown } | null;
  if (!item) return false;
  if (item.kind === "synthetic") return typeof (item as { generationPrompt?: unknown }).generationPrompt === "string";
  if (item.kind === "imported") return typeof (item as { source?: unknown }).source === "string";
  return false;
}

function isExample(value: unknown): value is DatasetExample {
  const item = value as Partial<DatasetExample> | null;
  return Boolean(
    item &&
      typeof item.id === "string" &&
      typeof item.fields === "object" &&
      item.fields !== null &&
      isProvenance(item.provenance) &&
      isPrivacyResult(item.privacy) &&
      ["none", "exact", "near"].includes(item.duplicateKind ?? "") &&
      typeof item.included === "boolean" &&
      typeof item.version === "number" &&
      typeof item.createdAt === "number"
  );
}

function isEvalRun(value: unknown): value is EvalRunResult {
  const item = value as Partial<EvalRunResult> | null;
  return Boolean(
    item &&
      typeof item.id === "string" &&
      typeof item.version === "number" &&
      typeof item.createdAt === "number" &&
      typeof item.passed === "number" &&
      typeof item.total === "number" &&
      typeof item.summary === "string"
  );
}

function isDataset(value: unknown): value is GoldenDataset {
  const item = value as Partial<GoldenDataset> | null;
  return Boolean(
    item &&
      typeof item.id === "string" &&
      typeof item.name === "string" &&
      typeof item.seedDescription === "string" &&
      Array.isArray(item.fields) &&
      item.fields.every((f) => typeof f === "string") &&
      Array.isArray(item.examples) &&
      item.examples.every(isExample) &&
      Array.isArray(item.versions) &&
      Array.isArray(item.evalRuns) &&
      item.evalRuns.every(isEvalRun) &&
      typeof item.currentVersion === "number" &&
      typeof item.createdAt === "number" &&
      typeof item.updatedAt === "number"
  );
}

function hydrateDatasets(): GoldenDataset[] {
  try {
    const raw = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null") as { version?: unknown; datasets?: unknown } | null;
    if (raw?.version !== STORAGE_VERSION || !Array.isArray(raw.datasets)) return [];
    return raw.datasets.filter(isDataset);
  } catch {
    return [];
  }
}

function touch(dataset: GoldenDataset): GoldenDataset {
  return { ...dataset, updatedAt: Date.now() };
}

/** Recomputes duplicate status across the whole example list (oldest-first,
 * so the earliest copy always survives as canonical) and re-derives
 * inclusion — the single place a dataset's examples get folded back
 * together after any edit. */
function withRecomputedDuplicates(dataset: GoldenDataset): GoldenDataset {
  const ordered = [...dataset.examples].sort((a, b) => a.createdAt - b.createdAt);
  return { ...dataset, examples: recomputeDuplicates(ordered) };
}

export interface GoldenDatasetBuilderStore {
  datasets: GoldenDataset[];
  activeDatasetId: string | null;
  generating: boolean;
  setActiveDataset: (datasetId: string | null) => void;
  createDataset: (name: string, seedDescription: string, fieldsInput: string) => string;
  deleteDataset: (datasetId: string) => void;
  renameDataset: (datasetId: string, name: string) => void;
  updateSeedDescription: (datasetId: string, seedDescription: string) => void;
  /** Generates `count` new synthetic examples via the active model target,
   * merges them into the dataset, recomputes duplicates across the whole
   * set, and records a new version entry. */
  generateExamples: (datasetId: string, count: number) => Promise<void>;
  /** Imports pasted "real" examples, running the exact same privacy filter
   * synthetic examples go through before any of them are allowed into the
   * dataset — a real example that fails is flagged and excluded, never
   * silently included. Returns how many lines/rows didn't parse against the
   * schema at all (as opposed to parsing but failing the privacy filter). */
  importExamples: (datasetId: string, rawText: string, sourceLabel: string) => { imported: number; skippedLines: number };
  deleteExample: (datasetId: string, exampleId: string) => void;
  runEval: (datasetId: string) => void;
}

function activeModelTarget() {
  return resolveTarget();
}

export const useGoldenDatasetBuilderStore = create<GoldenDatasetBuilderStore>((set, get) => ({
  datasets: hydrateDatasets(),
  activeDatasetId: null,
  generating: false,

  setActiveDataset: (datasetId) => set({ activeDatasetId: datasetId }),

  createDataset: (name, seedDescription, fieldsInput) => {
    const fields = parseFieldsInput(fieldsInput);
    const dataset: GoldenDataset = {
      id: newDatasetId(),
      name: name.trim() || "Untitled dataset",
      seedDescription: seedDescription.trim(),
      fields,
      examples: [],
      versions: [newVersionEntry(1, "Dataset created", 0)],
      currentVersion: 1,
      evalRuns: [],
      createdAt: Date.now(),
      updatedAt: Date.now(),
      lastError: null,
    };
    const datasets = [dataset, ...get().datasets];
    persist(datasets);
    set({ datasets, activeDatasetId: dataset.id });
    return dataset.id;
  },

  deleteDataset: (datasetId) => {
    const datasets = get().datasets.filter((dataset) => dataset.id !== datasetId);
    persist(datasets);
    set((state) => ({ datasets, activeDatasetId: state.activeDatasetId === datasetId ? null : state.activeDatasetId }));
  },

  renameDataset: (datasetId, name) => {
    const trimmed = name.trim();
    if (!trimmed) return;
    const datasets = get().datasets.map((dataset) => (dataset.id === datasetId ? touch({ ...dataset, name: trimmed }) : dataset));
    persist(datasets);
    set({ datasets });
  },

  updateSeedDescription: (datasetId, seedDescription) => {
    const datasets = get().datasets.map((dataset) =>
      dataset.id === datasetId ? touch({ ...dataset, seedDescription }) : dataset
    );
    persist(datasets);
    set({ datasets });
  },

  generateExamples: async (datasetId, count) => {
    const dataset = get().datasets.find((candidate) => candidate.id === datasetId);
    if (!dataset) throw new Error("This dataset no longer exists.");
    if (get().generating) throw new Error("A generation run is already in progress.");
    if (!dataset.fields.length) throw new Error("Add at least one schema field before generating examples.");

    set({ generating: true });
    try {
      const target = await activeModelTarget();
      const callModel = async (messages: ChatMessage[], signal: AbortSignal): Promise<ModelCallResult> => {
        const result = await attemptStream(target, messages, [], signal, effortForTarget(target), GOLDEN_DATASET_SESSION_ID, undefined, false);
        return { content: result.content, streamError: result.streamError };
      };
      const { examples: generated, prompt } = await generateSyntheticExamples(dataset.seedDescription, dataset.fields, count, callModel);

      set((state) => ({
        datasets: state.datasets.map((candidate) => {
          if (candidate.id !== datasetId) return candidate;
          const nextVersion = candidate.currentVersion + 1;
          const materialized = generated.map((fields) =>
            materializeExample(fields, { kind: "synthetic", generationPrompt: prompt }, nextVersion)
          );
          const withNewExamples = { ...candidate, examples: [...candidate.examples, ...materialized] };
          const deduped = withRecomputedDuplicates(withNewExamples);
          return touch({
            ...deduped,
            currentVersion: nextVersion,
            versions: [
              ...deduped.versions,
              newVersionEntry(nextVersion, `Generated ${materialized.length} synthetic example(s)`, deduped.examples.length),
            ],
            lastError: null,
          });
        }),
      }));
      persist(get().datasets);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      set((state) => ({
        datasets: state.datasets.map((candidate) => (candidate.id === datasetId ? touch({ ...candidate, lastError: message }) : candidate)),
      }));
      persist(get().datasets);
      throw error;
    } finally {
      set({ generating: false });
    }
  },

  importExamples: (datasetId, rawText, sourceLabel) => {
    const dataset = get().datasets.find((candidate) => candidate.id === datasetId);
    if (!dataset) throw new Error("This dataset no longer exists.");
    if (!dataset.fields.length) throw new Error("Add at least one schema field before importing examples.");

    const { examples: parsed, skippedLines } = parseImportedExamples(rawText, dataset.fields);
    if (parsed.length === 0) {
      return { imported: 0, skippedLines };
    }

    const nextVersion = dataset.currentVersion + 1;
    const trimmedSource = sourceLabel.trim() || "Imported examples";
    const materialized = parsed.map((fields) => materializeExample(fields, { kind: "imported", source: trimmedSource }, nextVersion));

    const datasets = get().datasets.map((candidate) => {
      if (candidate.id !== datasetId) return candidate;
      const withNewExamples = { ...candidate, examples: [...candidate.examples, ...materialized] };
      const deduped = withRecomputedDuplicates(withNewExamples);
      const excludedByPrivacy = materialized.filter((example) => example.exclusionReason === "privacy").length;
      const note =
        `Imported ${materialized.length} example(s) from "${trimmedSource}"` +
        (excludedByPrivacy > 0 ? ` (${excludedByPrivacy} excluded by the privacy filter)` : "");
      return touch({
        ...deduped,
        currentVersion: nextVersion,
        versions: [...deduped.versions, newVersionEntry(nextVersion, note, deduped.examples.length)],
        lastError: null,
      });
    });
    persist(datasets);
    set({ datasets });
    return { imported: materialized.length, skippedLines };
  },

  deleteExample: (datasetId, exampleId) => {
    const dataset = get().datasets.find((candidate) => candidate.id === datasetId);
    if (!dataset) return;
    const nextVersion = dataset.currentVersion + 1;
    const datasets = get().datasets.map((candidate) => {
      if (candidate.id !== datasetId) return candidate;
      const remaining = candidate.examples.filter((example) => example.id !== exampleId);
      const deduped = withRecomputedDuplicates({ ...candidate, examples: remaining });
      return touch({
        ...deduped,
        currentVersion: nextVersion,
        versions: [...deduped.versions, newVersionEntry(nextVersion, "Removed an example", deduped.examples.length)],
      });
    });
    persist(datasets);
    set({ datasets });
  },

  runEval: (datasetId) => {
    const dataset = get().datasets.find((candidate) => candidate.id === datasetId);
    if (!dataset) throw new Error("This dataset no longer exists.");
    const result = runSchemaConformanceEval(dataset);
    const datasets = get().datasets.map((candidate) =>
      candidate.id === datasetId ? touch({ ...candidate, evalRuns: [result, ...candidate.evalRuns] }) : candidate
    );
    persist(datasets);
    set({ datasets });
  },
}));
