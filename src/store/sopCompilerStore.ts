/**
 * Draft compilations produced by the SOP-to-Agent Compiler (ROADMAP Phase 7,
 * item 24). Holds imported/pasted source text plus every compiled
 * `CompiledWorkflowDraft`, persisted the same way `skillProposalStore.ts`
 * persists its own quarantined proposals (plain `localStorage`, no Rust
 * command — this feature needs no new backend primitive). The ONLY thing a
 * draft here can ever become is a `quarantined` `SkillProposal` via
 * `sendToReview`, which calls `skillProposalStore.ts`'s EXISTING
 * `createProposal` unchanged — nothing in this store installs, activates, or
 * runs a compiled draft itself.
 */
import { create } from "zustand";
import { open } from "@tauri-apps/plugin-dialog";
import { readTextFile, stat } from "@tauri-apps/plugin-fs";

import { resolveTarget } from "../lib/agentLoop";
import { attemptStream } from "../lib/turnEngine";
import type { ChatMessage } from "../lib/llamaClient";
import { effortForTarget } from "./modelStore";
import {
  compileSop,
  renderCompiledSkillInstructions,
  type CompiledWorkflowDraft,
  type SopCompilerCallResult,
} from "../lib/sopCompiler";
import { useSkillProposalStore, type SkillProposal } from "./skillProposalStore";
import { errorMessage } from "../lib/errors";

const STORAGE_KEY = "little-monkey-sop-compiler-drafts-v1";
/** Fixed pseudo-session id for the compiler's one-shot model call — this run
 * never belongs to a chat session, and `recordUsage: false` below means
 * `attemptStream` never actually writes anything into `useUsageStore` under
 * it; it only needs to be a stable, non-empty string. */
const SOP_COMPILER_SESSION_ID = "sop-compiler";
/** Cap on the source excerpt retained alongside a compiled draft (for
 * provenance/review) — the full source already went to the model once via
 * `compileSop`'s own truncation; this is just what gets persisted and shown
 * back to the reviewer. */
const MAX_RETAINED_EXCERPT_CHARS = 8_000;
/** Reject an imported file above this size outright — mirrors
 * `EcosystemPackages.tsx`'s own `stat().size` guard on file import. */
const MAX_IMPORT_FILE_BYTES = 5 * 1024 * 1024;

export type SopDraftStatus = "draft" | "sent_for_review";

export interface SopCompilationDraft {
  id: string;
  sourceFileName: string | null;
  sourceExcerpt: string;
  draft: CompiledWorkflowDraft;
  createdAt: number;
  status: SopDraftStatus;
  proposalId: string | null;
}

interface SopCompilerStore {
  sourceText: string;
  sourceFileName: string | null;
  compiling: boolean;
  importing: boolean;
  error: string | null;
  drafts: SopCompilationDraft[];
  selectedDraftId: string | null;

  setSourceText: (text: string) => void;
  importFromFile: () => Promise<void>;
  compile: () => Promise<void>;
  selectDraft: (id: string | null) => void;
  discardDraft: (id: string) => void;
  sendToReview: (id: string) => Promise<SkillProposal>;
}

function persist(drafts: SopCompilationDraft[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ version: 1, drafts }));
  } catch {
    // The draft remains live in memory for the rest of this session even if
    // persistence fails (private-browsing-style storage denial, quota, etc).
  }
}

function isCompiledWorkflowDraftShape(value: unknown): value is CompiledWorkflowDraft {
  if (!value || typeof value !== "object") return false;
  const draft = value as Partial<CompiledWorkflowDraft>;
  return (
    typeof draft.name === "string" &&
    typeof draft.summary === "string" &&
    typeof draft.suggestedCommand === "string" &&
    Array.isArray(draft.steps) &&
    Array.isArray(draft.inputs) &&
    Array.isArray(draft.policyGates) &&
    Array.isArray(draft.tests) &&
    Array.isArray(draft.evidence)
  );
}

function hydrate(): SopCompilationDraft[] {
  try {
    const raw = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null") as
      | { version?: unknown; drafts?: unknown }
      | null;
    if (raw?.version !== 1 || !Array.isArray(raw.drafts)) return [];
    return raw.drafts.filter((value): value is SopCompilationDraft => {
      const entry = value as Partial<SopCompilationDraft>;
      return Boolean(
        entry &&
        typeof entry.id === "string" &&
        typeof entry.sourceExcerpt === "string" &&
        (entry.sourceFileName === null || typeof entry.sourceFileName === "string") &&
        isCompiledWorkflowDraftShape(entry.draft) &&
        typeof entry.createdAt === "number" &&
        ["draft", "sent_for_review"].includes(entry.status ?? "") &&
        (entry.proposalId === null || typeof entry.proposalId === "string"),
      );
    });
  } catch {
    return [];
  }
}

export const useSopCompilerStore = create<SopCompilerStore>((set, get) => ({
  sourceText: "",
  sourceFileName: null,
  compiling: false,
  importing: false,
  error: null,
  drafts: hydrate(),
  selectedDraftId: null,

  setSourceText: (text) => set({ sourceText: text, error: null }),

  importFromFile: async () => {
    set({ importing: true, error: null });
    try {
      const selected = await open({
        title: "Import SOP, runbook, checklist, or training document",
        multiple: false,
        directory: false,
        filters: [
          { name: "Text documents", extensions: ["txt", "md", "markdown"] },
        ],
      });
      if (typeof selected !== "string") {
        set({ importing: false });
        return;
      }
      const fileInfo = await stat(selected);
      if (fileInfo.size > MAX_IMPORT_FILE_BYTES) {
        throw new Error(`That file is larger than ${Math.floor(MAX_IMPORT_FILE_BYTES / (1024 * 1024))}MB — paste the relevant excerpt instead.`);
      }
      const content = await readTextFile(selected);
      const fileName = selected.split(/[\\/]/).pop() ?? selected;
      set({ sourceText: content, sourceFileName: fileName, importing: false });
    } catch (err) {
      set({ importing: false, error: errorMessage(err) });
    }
  },

  compile: async () => {
    const { sourceText, sourceFileName } = get();
    if (!sourceText.trim()) {
      set({ error: "Paste or import an SOP, runbook, checklist, or training document before compiling." });
      return;
    }
    set({ compiling: true, error: null });
    try {
      const target = await resolveTarget();
      const callModel = async (messages: ChatMessage[], signal?: AbortSignal): Promise<SopCompilerCallResult> => {
        const result = await attemptStream(
          target,
          messages,
          [],
          signal,
          effortForTarget(target),
          SOP_COMPILER_SESSION_ID,
          undefined,
          false,
        );
        return { content: result.content, streamError: result.streamError };
      };
      const compiled = await compileSop(sourceText, callModel, sourceFileName ?? undefined);
      const entry: SopCompilationDraft = {
        id: crypto.randomUUID(),
        sourceFileName,
        sourceExcerpt: sourceText.trim().slice(0, MAX_RETAINED_EXCERPT_CHARS),
        draft: compiled,
        createdAt: Date.now(),
        status: "draft",
        proposalId: null,
      };
      const drafts = [entry, ...get().drafts];
      persist(drafts);
      set({ drafts, selectedDraftId: entry.id, compiling: false });
    } catch (err) {
      set({ compiling: false, error: errorMessage(err) });
    }
  },

  selectDraft: (id) => set({ selectedDraftId: id }),

  discardDraft: (id) => {
    const drafts = get().drafts.filter((entry) => entry.id !== id);
    persist(drafts);
    set((state) => ({
      drafts,
      selectedDraftId: state.selectedDraftId === id ? null : state.selectedDraftId,
    }));
  },

  /**
   * The ONLY hand-off out of this store: forwards the compiled draft,
   * rendered as skill instructions, into `skillProposalStore.ts`'s existing
   * `createProposal` — the same quarantined-until-approved review flow every
   * other proposal (e.g. `/learn`) already goes through. This function never
   * installs, enables, or runs anything itself.
   */
  sendToReview: async (id) => {
    const entry = get().drafts.find((draft) => draft.id === id);
    if (!entry) throw new Error("Compiled draft not found.");
    if (entry.status === "sent_for_review") {
      throw new Error("This draft was already sent to Skill Proposals for review.");
    }
    const instructions = renderCompiledSkillInstructions(entry.draft, entry.sourceExcerpt);
    const proposal = await useSkillProposalStore.getState().createProposal(entry.draft.suggestedCommand, instructions);
    const drafts = get().drafts.map((draft) =>
      draft.id === id ? { ...draft, status: "sent_for_review" as const, proposalId: proposal.id } : draft,
    );
    persist(drafts);
    set({ drafts });
    return proposal;
  },
}));
