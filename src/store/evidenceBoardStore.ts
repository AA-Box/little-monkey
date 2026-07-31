import { invoke } from "@tauri-apps/api/core";
import { create } from "zustand";

import {
  assistantTextFromMessages,
  extractClaims,
  materializeClaim,
  newBoardId,
  type Claim,
  type ClaimStatus,
  type EvidenceBoard,
} from "../lib/evidenceBoard";
import { beginDurableRun, defaultRunBudgets, type DurableRunRecorder } from "../lib/durableRun";
import {
  buildModelTargetInventory,
  findActiveModelTarget,
  isModelTargetSnapshot,
  type ModelTargetSnapshot,
} from "../lib/modelTargets";
import { registerRunCancellation } from "../lib/runCancellationRegistry";
import { attemptStream, type ResolvedTarget } from "../lib/turnEngine";
import { useModelStore } from "./modelStore";
import { usePermissionStore } from "./permissionStore";
import { useSessionStore } from "./sessionStore";
import { useWorkspaceStore } from "./workspaceStore";
import { errorMessage } from "../lib/errors";

/**
 * Evidence Board and Claim Checker (ROADMAP.md Phase 7, item 6): holds
 * named boards of claims extracted from a chat session or a pasted report,
 * so a user can audit a generated report claim-by-claim — each with its own
 * confidence, grounded evidence, owner, and status — instead of trusting
 * one summary wholesale. `../lib/evidenceBoard.ts` owns the actual
 * extraction/grounding logic (pure, no React/zustand); this store owns
 * board persistence plus the real model round trip, built the exact same
 * `attemptStream`-against-the-active-target way `translation.ts` builds its
 * own one-shot, no-tools calls (see that module's `resolveTarget`).
 *
 * Persisted to localStorage (not zustand's `persist` middleware) with the
 * same hand-rolled versioned-envelope shape `skillProposalStore.ts` uses —
 * boards are lightweight, so a corrupt/incompatible payload is simply
 * dropped on hydrate rather than crashing the app.
 */

const STORAGE_KEY = "little-monkey-evidence-boards-v1";
const STORAGE_VERSION = 1;

function persistBoards(boards: EvidenceBoard[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ version: STORAGE_VERSION, boards }));
  } catch {
    // Best-effort only — boards stay live in memory for the rest of this session.
  }
}

function isClaim(value: unknown): value is Claim {
  const item = value as Partial<Claim> | null;
  return Boolean(
    item &&
      typeof item.id === "string" &&
      typeof item.text === "string" &&
      (item.confidence === "high" || item.confidence === "medium" || item.confidence === "low") &&
      Array.isArray(item.supportingEvidence) &&
      Array.isArray(item.conflictingEvidence) &&
      typeof item.unresolved === "boolean" &&
      typeof item.owner === "string" &&
      typeof item.status === "string" &&
      typeof item.createdAt === "number"
  );
}

function isBoard(value: unknown): value is EvidenceBoard {
  const item = value as Partial<EvidenceBoard> | null;
  return Boolean(
    item &&
      typeof item.id === "string" &&
      typeof item.name === "string" &&
      (item.sourceKind === "session" || item.sourceKind === "pasted") &&
      typeof item.sourceText === "string" &&
      Array.isArray(item.claims) &&
      item.claims.every(isClaim) &&
      typeof item.createdAt === "number" &&
      typeof item.updatedAt === "number"
  );
}

function hydrateBoards(): EvidenceBoard[] {
  try {
    const raw = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null") as { version?: unknown; boards?: unknown } | null;
    if (raw?.version !== STORAGE_VERSION || !Array.isArray(raw.boards)) return [];
    return raw.boards.filter(isBoard);
  } catch {
    return [];
  }
}

export interface EvidenceBoardStore {
  boards: EvidenceBoard[];
  activeBoardId: string | null;
  extracting: boolean;
  setActiveBoard: (boardId: string | null) => void;
  /** Finds (or creates, if none exists yet) the board tracking a session,
   * makes it active, and returns its id — the panel calls this once on
   * open with whatever session is currently active in chat. */
  openSessionBoard: (sessionId: string, sessionTitle: string) => string;
  /** Creates a brand-new board from pasted text and makes it active. */
  createPastedBoard: (name: string, text: string) => string;
  deleteBoard: (boardId: string) => void;
  renameBoard: (boardId: string, name: string) => void;
  /** Re-runs extraction against the board's live source (re-reading the
   * session's current messages for a session board) and merges any newly
   * found claims in by text, keeping existing claims (and their
   * owner/status edits) untouched. */
  runExtraction: (boardId: string) => Promise<void>;
  updateClaimOwner: (boardId: string, claimId: string, owner: string) => void;
  updateClaimStatus: (boardId: string, claimId: string, status: ClaimStatus) => void;
  deleteClaim: (boardId: string, claimId: string) => void;
}

function touch(board: EvidenceBoard): EvidenceBoard {
  return { ...board, updatedAt: Date.now() };
}

function currentSourceText(board: EvidenceBoard): string {
  if (board.sourceKind === "session" && board.sourceSessionId) {
    const session = useSessionStore.getState().sessions.find((candidate) => candidate.id === board.sourceSessionId);
    if (!session) throw new Error("This conversation no longer exists.");
    const text = assistantTextFromMessages(session.messages);
    if (!text.trim()) throw new Error("This conversation has no assistant messages to extract claims from yet.");
    return text;
  }
  if (!board.sourceText.trim()) throw new Error("There is no text to extract claims from.");
  return board.sourceText;
}

interface LlamaStatusResult {
  status: "stopped" | "starting" | "ready" | "error";
  port: number;
  model_path: string | null;
}

function activeModelTarget(sourceSessionId: string | null): ModelTargetSnapshot {
  const modelState = useModelStore.getState();
  if (sourceSessionId) {
    const session = useSessionStore.getState().sessions.find((candidate) => candidate.id === sourceSessionId);
    if (session?.modelTarget && isModelTargetSnapshot(session.modelTarget)) {
      return structuredClone(session.modelTarget);
    }
  }
  const inventory = buildModelTargetInventory({
    installed: modelState.installed,
    active: modelState.active,
    llamaStatus: modelState.llamaStatus,
    ollamaModels: modelState.ollamaModels,
    ollamaReachable: modelState.ollamaReachable,
    providers: modelState.providers,
    providerModels: modelState.providerModels,
    effortByTarget: modelState.effortByTarget,
  });
  const target = findActiveModelTarget(inventory, modelState);
  if (!target) throw new Error("Select and connect a chat model before extracting claims.");
  return structuredClone(target);
}

/** Resolves a frozen model-target snapshot down to the transport shape
 * `attemptStream` needs — identical logic to `translation.ts`'s own
 * (unexported) `resolveTarget`, kept as its own small copy here for the
 * same reason `riskJudge.ts`/`subagent.ts` each keep their own: this module
 * must not import from `translation.ts`, and the shape is a handful of
 * lines, not worth threading through a shared export. */
async function resolveTarget(target: ModelTargetSnapshot): Promise<ResolvedTarget> {
  if (target.kind === "provider") return { kind: "provider", providerId: target.providerId, model: target.model };
  if (target.kind === "ollama") return { kind: "ollama", baseUrl: target.baseUrl, model: target.model };
  const status = await invoke<LlamaStatusResult>("llama_status");
  if (status.status !== "ready" || status.model_path !== target.modelPath) {
    throw new Error(`${target.displayName} is no longer loaded in the managed llama.cpp runtime.`);
  }
  return { kind: "local", baseUrl: `http://127.0.0.1:${status.port}`, modelLabel: target.displayName };
}

async function beginExtractionRun(runId: string, target: ModelTargetSnapshot, task: string): Promise<DurableRunRecorder | null> {
  return beginDurableRun({
    runId,
    kind: "interactive",
    task,
    instructions: "Claim extraction: no tools, source-grounded evidence spans only",
    target,
    roots: useWorkspaceStore.getState().roots,
    workspaceAccess: "read_only",
    permissionMode: usePermissionStore.getState().mode,
    allowNetwork: target.kind === "provider" || (target.kind === "ollama" && target.isCloud === true),
    budgets: { ...defaultRunBudgets(true), max_model_calls: 1, max_iterations: 1 },
  });
}

export const useEvidenceBoardStore = create<EvidenceBoardStore>((set, get) => ({
  boards: hydrateBoards(),
  activeBoardId: null,
  extracting: false,

  setActiveBoard: (boardId) => set({ activeBoardId: boardId }),

  openSessionBoard: (sessionId, sessionTitle) => {
    const existing = get().boards.find((board) => board.sourceKind === "session" && board.sourceSessionId === sessionId);
    if (existing) {
      set({ activeBoardId: existing.id });
      return existing.id;
    }
    const board: EvidenceBoard = {
      id: newBoardId(),
      name: sessionTitle.trim() || "Untitled conversation",
      sourceKind: "session",
      sourceSessionId: sessionId,
      sourceText: "",
      sourceTruncated: false,
      claims: [],
      createdAt: Date.now(),
      updatedAt: Date.now(),
      lastExtractionError: null,
    };
    const boards = [board, ...get().boards];
    persistBoards(boards);
    set({ boards, activeBoardId: board.id });
    return board.id;
  },

  createPastedBoard: (name, text) => {
    const board: EvidenceBoard = {
      id: newBoardId(),
      name: name.trim() || "Pasted report",
      sourceKind: "pasted",
      sourceSessionId: null,
      sourceText: text,
      sourceTruncated: false,
      claims: [],
      createdAt: Date.now(),
      updatedAt: Date.now(),
      lastExtractionError: null,
    };
    const boards = [board, ...get().boards];
    persistBoards(boards);
    set({ boards, activeBoardId: board.id });
    return board.id;
  },

  deleteBoard: (boardId) => {
    const boards = get().boards.filter((board) => board.id !== boardId);
    persistBoards(boards);
    set((state) => ({ boards, activeBoardId: state.activeBoardId === boardId ? null : state.activeBoardId }));
  },

  renameBoard: (boardId, name) => {
    const trimmed = name.trim();
    if (!trimmed) return;
    const boards = get().boards.map((board) => (board.id === boardId ? touch({ ...board, name: trimmed }) : board));
    persistBoards(boards);
    set({ boards });
  },

  runExtraction: async (boardId) => {
    const board = get().boards.find((candidate) => candidate.id === boardId);
    if (!board) throw new Error("This board no longer exists.");
    if (get().extracting) throw new Error("An extraction is already running.");

    set({ extracting: true });
    const runId = `evidence-board-${crypto.randomUUID()}`;
    let recorder: DurableRunRecorder | null = null;
    const controller = new AbortController();
    const unregister = registerRunCancellation(runId, () => controller.abort());
    try {
      const sourceText = currentSourceText(board);
      const target = activeModelTarget(board.sourceSessionId);
      const resolved = await resolveTarget(target);
      recorder = await beginExtractionRun(runId, target, `Extract claims for board "${board.name}"`);

      const { claims: extracted, truncated } = await extractClaims(
        sourceText,
        async (messages, signal) => {
          const result = await attemptStream(resolved, messages, [], signal, target.effort, boardId, undefined, false, 4_096, runId);
          if (result.usage) recorder?.recordUsage(result.usage.promptTokens, result.usage.completionTokens);
          return { content: result.content, streamError: result.streamError };
        },
        controller.signal
      );

      set((state) => ({
        boards: state.boards.map((candidate) => {
          if (candidate.id !== boardId) return candidate;
          const existingByText = new Map(candidate.claims.map((claim) => [claim.text, claim]));
          const mergedClaims: Claim[] = extracted.map((entry) => {
            const existing = existingByText.get(entry.text);
            if (existing) {
              // Preserve the user's own owner/status edits across a
              // re-extraction that happens to find the exact same claim
              // text again; refresh the evidence/confidence/unresolved fields.
              return { ...existing, ...entry };
            }
            return materializeClaim(entry);
          });
          return touch({
            ...candidate,
            sourceText,
            sourceTruncated: truncated,
            claims: mergedClaims,
            lastExtractionError: null,
          });
        }),
      }));
      persistBoards(get().boards);
      await recorder?.complete(`Extracted ${extracted.length} claim(s).`);
    } catch (error) {
      const message = errorMessage(error);
      set((state) => ({
        boards: state.boards.map((candidate) => (candidate.id === boardId ? touch({ ...candidate, lastExtractionError: message }) : candidate)),
      }));
      persistBoards(get().boards);
      if (controller.signal.aborted) await recorder?.cancel("Extraction cancelled");
      else await recorder?.fail(error);
      throw error;
    } finally {
      unregister();
      set({ extracting: false });
    }
  },

  updateClaimOwner: (boardId, claimId, owner) => {
    const boards = get().boards.map((board) => {
      if (board.id !== boardId) return board;
      return touch({ ...board, claims: board.claims.map((claim) => (claim.id === claimId ? { ...claim, owner } : claim)) });
    });
    persistBoards(boards);
    set({ boards });
  },

  updateClaimStatus: (boardId, claimId, status) => {
    const boards = get().boards.map((board) => {
      if (board.id !== boardId) return board;
      return touch({ ...board, claims: board.claims.map((claim) => (claim.id === claimId ? { ...claim, status } : claim)) });
    });
    persistBoards(boards);
    set({ boards });
  },

  deleteClaim: (boardId, claimId) => {
    const boards = get().boards.map((board) => {
      if (board.id !== boardId) return board;
      return touch({ ...board, claims: board.claims.filter((claim) => claim.id !== claimId) });
    });
    persistBoards(boards);
    set({ boards });
  },
}));
