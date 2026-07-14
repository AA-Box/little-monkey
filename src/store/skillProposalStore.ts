import { create } from "zustand";

import { BUILT_IN_SLASH_COMMANDS } from "../lib/slashCommands";
import { usePromptStore } from "./promptStore";

const STORAGE_KEY = "little-monkey-skill-proposals-v1";
const BUILT_INS = new Set<string>(BUILT_IN_SLASH_COMMANDS.map((entry) => entry.command));

export type SkillProposalStatus = "quarantined" | "applied" | "rejected" | "rolled_back";

export interface SkillProposal {
  id: string;
  command: string;
  name: string;
  instructions: string;
  contentSha256: string;
  riskFlags: string[];
  status: SkillProposalStatus;
  createdAt: number;
  reviewedAt: number | null;
  appliedPromptId: string | null;
}

interface SkillProposalStore {
  proposals: SkillProposal[];
  createProposal: (command: string, instructions: string) => Promise<SkillProposal>;
  approveProposal: (id: string, expectedSha256: string) => Promise<SkillProposal>;
  rejectProposal: (id: string) => void;
  rollbackProposal: (id: string) => void;
}

function persist(proposals: SkillProposal[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ version: 1, proposals }));
  } catch {
    // The proposal remains live in memory. The existing prompt store reports
    // its own persistence failures when an approved skill is activated.
  }
}

function hydrate(): SkillProposal[] {
  try {
    const raw = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null") as { version?: unknown; proposals?: unknown } | null;
    if (raw?.version !== 1 || !Array.isArray(raw.proposals)) return [];
    return raw.proposals.filter((value): value is SkillProposal => {
      const item = value as Partial<SkillProposal>;
      return Boolean(
        item &&
        typeof item.id === "string" &&
        typeof item.command === "string" &&
        typeof item.name === "string" &&
        typeof item.instructions === "string" &&
        typeof item.contentSha256 === "string" &&
        Array.isArray(item.riskFlags) &&
        ["quarantined", "applied", "rejected", "rolled_back"].includes(item.status ?? "") &&
        typeof item.createdAt === "number",
      );
    });
  } catch {
    return [];
  }
}

export async function sha256Text(value: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function validate(command: string, instructions: string): { command: string; instructions: string } {
  const normalized = command.trim().toLowerCase();
  const body = instructions.trim();
  if (!/^[a-z0-9][a-z0-9-]{0,31}$/.test(normalized)) {
    throw new Error("Skill command must use 1-32 lowercase letters, digits, or hyphens.");
  }
  if (BUILT_INS.has(normalized)) throw new Error(`/${normalized} is reserved by a built-in command.`);
  if (body.length < 8 || body.length > 100_000) {
    throw new Error("Skill instructions must contain between 8 and 100,000 characters.");
  }
  return { command: normalized, instructions: body };
}

function riskFlags(instructions: string): string[] {
  const checks: Array<[RegExp, string]> = [
    [/<\|(?:system|assistant|user|tool)[^>]*\|>/i, "Contains model role/control tokens"],
    [/\b(?:ignore|override|bypass)\b.{0,40}\b(?:permission|policy|instruction|system)\b/is, "Attempts to override instructions or permissions"],
    [/\b(?:curl|wget|powershell|bash|sh)\b.{0,80}(?:https?:\/\/|-[ce])/is, "Requests command or network execution"],
    [/\b(?:password|api[_ -]?key|token|secret|keychain)\b/i, "References credentials or secrets"],
    [/\b(?:rm\s+-rf|format\s+[a-z]:|delete\s+all)\b/i, "Contains destructive-operation language"],
  ];
  return checks.filter(([pattern]) => pattern.test(instructions)).map(([, label]) => label);
}

export const useSkillProposalStore = create<SkillProposalStore>((set, get) => ({
  proposals: hydrate(),

  createProposal: async (rawCommand, rawInstructions) => {
    const { command, instructions } = validate(rawCommand, rawInstructions);
    if (usePromptStore.getState().entries.some((entry) => entry.command.toLowerCase() === command)) {
      throw new Error(`/${command} already exists in the prompt library.`);
    }
    const contentSha256 = await sha256Text(instructions);
    const proposal: SkillProposal = {
      id: crypto.randomUUID(),
      command,
      name: command.split("-").map((part) => part.charAt(0).toUpperCase() + part.slice(1)).join(" "),
      instructions,
      contentSha256,
      riskFlags: riskFlags(instructions),
      status: "quarantined",
      createdAt: Date.now(),
      reviewedAt: null,
      appliedPromptId: null,
    };
    const proposals = [proposal, ...get().proposals];
    persist(proposals);
    set({ proposals });
    return proposal;
  },

  approveProposal: async (id, expectedSha256) => {
    const proposal = get().proposals.find((entry) => entry.id === id);
    if (!proposal || proposal.status !== "quarantined") throw new Error("Skill proposal is not awaiting review.");
    const actual = await sha256Text(proposal.instructions);
    if (actual !== proposal.contentSha256 || actual !== expectedSha256) {
      throw new Error("Skill proposal changed after review; reopen it and verify the new digest.");
    }
    if (proposal.riskFlags.length > 0) {
      // The review UI requires a second explicit confirmation for flagged
      // proposals before it calls this action. The digest remains the final
      // backend-independent authorization binding.
    }
    const created = usePromptStore.getState().addEntry({
      kind: "skill",
      name: proposal.name,
      command: proposal.command,
      content: proposal.instructions,
      description: "Created from a reviewed /learn proposal.",
    });
    const applied: SkillProposal = {
      ...proposal,
      status: "applied",
      reviewedAt: Date.now(),
      appliedPromptId: created.id,
    };
    const proposals = get().proposals.map((entry) => entry.id === id ? applied : entry);
    persist(proposals);
    set({ proposals });
    return applied;
  },

  rejectProposal: (id) => {
    const proposals = get().proposals.map((entry) =>
      entry.id === id && entry.status === "quarantined"
        ? { ...entry, status: "rejected" as const, reviewedAt: Date.now() }
        : entry,
    );
    persist(proposals);
    set({ proposals });
  },

  rollbackProposal: (id) => {
    const proposal = get().proposals.find((entry) => entry.id === id);
    if (!proposal || proposal.status !== "applied" || !proposal.appliedPromptId) return;
    usePromptStore.getState().removeEntry(proposal.appliedPromptId);
    const proposals = get().proposals.map((entry) =>
      entry.id === id
        ? { ...entry, status: "rolled_back" as const, reviewedAt: Date.now(), appliedPromptId: null }
        : entry,
    );
    persist(proposals);
    set({ proposals });
  },
}));
