import { create } from "zustand";

import { nativeSkillsClient, type NativeSkillDescriptor } from "../lib/nativeSkillsClient";
import {
  scoreConnector,
  scoreLocalModel,
  scoreMcpServer,
  scoreOllamaModel,
  scorePlugin,
  scoreProviderModel,
  scoreSkill,
  scoreWorkflow,
  type ModelUsageLookup,
  type TrustScorecard,
} from "../lib/trustScorecards";
import { useConnectorsStore } from "./connectorsStore";
import { useEcosystemStore } from "./ecosystemStore";
import { useMcpStore } from "./mcpStore";
import { useModelStore } from "./modelStore";
import { useUsageHistoryStore } from "./usageHistoryStore";
import { errorMessage } from "../lib/errors";

function errorText(error: unknown): string {
  return errorMessage(error);
}

/**
 * Pure aggregation of `trustScorecards.ts`'s per-entity scorers over
 * whatever is CURRENTLY held in each already-existing store — this store
 * never calls a source store's own `refresh()`/`connect()`/etc. itself
 * (that stays the panel's job, exactly like `TrustScorecardsPanel.tsx`'s
 * mount effect calls `modelStore.refresh()`, `connectorsStore.refresh()`,
 * etc. before calling `recompute()` here). Keeping this store a pure reader
 * of already-hydrated state — plus one real `invoke()` for native skills,
 * which has no dedicated store of its own (see `nativeSkillsStore.ts`'s doc
 * comment) — is what makes "reads already-known metadata" true rather than
 * aspirational, and keeps this store trivially testable by seeding the
 * source stores' state directly.
 */
export interface TrustScorecardsStore {
  scorecards: TrustScorecard[];
  loading: boolean;
  error: string | null;
  lastComputedAt: number | null;
  /** Re-fetches native skills (the one entity kind with no store of its own)
   * and recomputes every scorecard from whatever the other stores currently
   * hold. Safe to call anytime; failures leave the previous scorecards in
   * place rather than clearing the panel to empty. */
  recompute: () => Promise<void>;
}

function usageLookup(): ModelUsageLookup {
  return { byModel: useUsageHistoryStore.getState().byModel };
}

export const useTrustScorecardsStore = create<TrustScorecardsStore>((set) => ({
  scorecards: [],
  loading: false,
  error: null,
  lastComputedAt: null,

  recompute: async () => {
    set({ loading: true, error: null });
    let skills: NativeSkillDescriptor[] = [];
    try {
      skills = await nativeSkillsClient.discover();
    } catch (err) {
      // A failed skill discovery shouldn't block scoring every other entity
      // kind — it just means the skill rows are missing this pass.
      set({ error: errorText(err) });
    }

    try {
      const modelState = useModelStore.getState();
      const usage = usageLookup();
      const connectors = useConnectorsStore.getState().accounts;
      const mcpServers = useMcpStore.getState().servers;
      const ecosystem = useEcosystemStore.getState();

      const scorecards: TrustScorecard[] = [];

      for (const model of modelState.installed) {
        scorecards.push(scoreLocalModel(model, usage, modelState.llamaStatus, modelState.active?.id === model.id));
      }
      for (const model of modelState.ollamaModels) {
        scorecards.push(scoreOllamaModel(model, usage));
      }
      for (const provider of modelState.providers) {
        const providerModels = modelState.providerModels[provider.id] ?? [];
        for (const providerModel of providerModels) {
          scorecards.push(scoreProviderModel(provider, providerModel.id, usage, modelState.providerKeyError));
        }
      }
      for (const account of connectors) {
        scorecards.push(scoreConnector(account));
      }
      for (const server of mcpServers) {
        scorecards.push(scoreMcpServer(server));
      }
      for (const skill of skills) {
        scorecards.push(scoreSkill(skill));
      }
      for (const workflow of ecosystem.workflows) {
        scorecards.push(scoreWorkflow(workflow, ecosystem.histories, ecosystem.catalog));
      }
      for (const plugin of ecosystem.plugins) {
        scorecards.push(scorePlugin(plugin, ecosystem.catalog));
      }

      set({ scorecards, loading: false, lastComputedAt: Date.now() });
    } catch (err) {
      set({ error: errorText(err), loading: false });
    }
  },
}));
