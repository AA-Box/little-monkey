import { create } from "zustand";

import {
  BUILTIN_FIXTURES,
  type RedTeamFixture,
  type TriggeredAction,
} from "../lib/redTeamFixtures";
import { runAllFixtures, runFixture, type FixtureRunResult } from "../lib/redTeamRunner";
import { usePermissionStore, type PermissionMode } from "./permissionStore";

const CUSTOM_FIXTURES_STORAGE_KEY = "little-monkey-redteam-custom-fixtures";

/** Draft shape for the panel's "add a custom fixture" form — every field is a
 * plain string so a `<textarea>`/`<input>` can bind directly; parsed/
 * validated into a real `RedTeamFixture` by `addFixture` below. */
export interface CustomFixtureDraft {
  title: string;
  sourceType: RedTeamFixture["sourceType"];
  simulatedToolName: string;
  isMcp: boolean;
  content: string;
  rawControlToken: string;
  triggeredActionTool: string;
  triggeredActionArgsJson: string;
  triggeredActionDescription: string;
  expectedOutcome: RedTeamFixture["expectedOutcome"];
}

function loadCustomFixtures(): RedTeamFixture[] {
  try {
    const raw = localStorage.getItem(CUSTOM_FIXTURES_STORAGE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(
      (f): f is RedTeamFixture => f && typeof f === "object" && typeof f.id === "string" && f.builtin === false,
    );
  } catch {
    return [];
  }
}

function persistCustomFixtures(fixtures: RedTeamFixture[]): void {
  try {
    localStorage.setItem(CUSTOM_FIXTURES_STORAGE_KEY, JSON.stringify(fixtures));
  } catch {
    // Best-effort persistence, same as permissionStore.ts's mode storage —
    // a failure here shouldn't break running the lab in-session.
  }
}

interface RedTeamStore {
  fixtures: RedTeamFixture[];
  results: Record<string, FixtureRunResult>;
  /** The permission mode the panel's selector is currently set to test
   * against — defaults to whatever mode is actually active in the app right
   * now (a real read of `permissionStore.ts`), not a hardcoded guess. */
  mode: PermissionMode;
  running: boolean;
  formError: string | null;

  setMode: (mode: PermissionMode) => void;
  runAll: () => Promise<void>;
  runOne: (id: string) => Promise<void>;
  clearResults: () => void;
  addFixture: (draft: CustomFixtureDraft) => boolean;
  removeFixture: (id: string) => void;
}

export const useRedTeamStore = create<RedTeamStore>((set, get) => ({
  fixtures: [...BUILTIN_FIXTURES, ...loadCustomFixtures()],
  results: {},
  mode: usePermissionStore.getState().mode,
  running: false,
  formError: null,

  setMode: (mode) => set({ mode }),

  // Async since the gate verdict now comes from the real Rust decision table
  // over IPC rather than a frontend copy of it.
  runAll: async () => {
    set({ running: true });
    try {
      const { fixtures, mode } = get();
      const runResults = await runAllFixtures(fixtures, mode);
      const results: Record<string, FixtureRunResult> = {};
      for (const result of runResults) results[result.fixtureId] = result;
      set({ results });
    } finally {
      set({ running: false });
    }
  },

  runOne: async (id) => {
    const { fixtures, mode } = get();
    const fixture = fixtures.find((f) => f.id === id);
    if (!fixture) return;
    const result = await runFixture(fixture, mode);
    set((state) => ({ results: { ...state.results, [id]: result } }));
  },

  clearResults: () => set({ results: {} }),

  addFixture: (draft) => {
    const title = draft.title.trim();
    const simulatedToolName = draft.simulatedToolName.trim();
    const content = draft.content;
    const triggeredActionTool = draft.triggeredActionTool.trim();
    const triggeredActionDescription = draft.triggeredActionDescription.trim();

    if (!title || !simulatedToolName || !content || !triggeredActionTool || !triggeredActionDescription) {
      set({ formError: "Title, simulated tool name, content, triggered tool, and description are all required." });
      return false;
    }

    let args: Record<string, unknown>;
    try {
      const parsed = draft.triggeredActionArgsJson.trim() === "" ? {} : JSON.parse(draft.triggeredActionArgsJson);
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
        throw new Error("must be a JSON object");
      }
      args = parsed as Record<string, unknown>;
    } catch {
      set({ formError: "Triggered action args must be valid JSON (e.g. {\"path\": \"src/index.ts\"})." });
      return false;
    }

    const triggeredAction: TriggeredAction = {
      tool: triggeredActionTool,
      args,
      description: triggeredActionDescription,
    };

    const newFixture: RedTeamFixture = {
      id: `custom-${crypto.randomUUID()}`,
      title,
      sourceType: draft.sourceType,
      simulatedToolName,
      isMcp: draft.isMcp,
      content,
      rawControlToken: draft.rawControlToken.trim() || undefined,
      triggeredAction,
      expectedOutcome: draft.expectedOutcome,
      builtin: false,
    };

    set((state) => {
      const fixtures = [...state.fixtures, newFixture];
      persistCustomFixtures(fixtures.filter((f) => !f.builtin));
      return { fixtures, formError: null };
    });
    return true;
  },

  removeFixture: (id) => {
    set((state) => {
      const target = state.fixtures.find((f) => f.id === id);
      if (!target || target.builtin) return state;
      const fixtures = state.fixtures.filter((f) => f.id !== id);
      const results = { ...state.results };
      delete results[id];
      persistCustomFixtures(fixtures.filter((f) => !f.builtin));
      return { fixtures, results };
    });
  },
}));
