import { create } from "zustand";
import { isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

import { skillLearningClient } from "../lib/skillLearningClient";
import type { NativeSkillDescriptor } from "../lib/nativeSkillsClient";

const NATIVE_SKILLS_CHANGED_EVENT = "native-skills://changed";

export interface NativeSkillsStore {
  descriptors: NativeSkillDescriptor[];
  loading: boolean;
  error: string | null;
  generation: number;
  refresh: () => Promise<void>;
  invalidate: (reason: string) => void;
}

let refreshPromise: Promise<void> | null = null;
let refreshQueued = false;

async function refreshNativeSkills(queueIfRunning = false): Promise<void> {
  if (refreshPromise) {
    if (queueIfRunning) refreshQueued = true;
    return refreshPromise;
  }

  refreshPromise = (async () => {
    useNativeSkillsStore.setState({ loading: true, error: null });
    let lastError: unknown = null;
    do {
      refreshQueued = false;
      try {
        const descriptors = await skillLearningClient.discover();
        useNativeSkillsStore.setState((state) => ({
          descriptors,
          loading: true,
          error: null,
          generation: state.generation + 1,
        }));
        lastError = null;
      } catch (error) {
        lastError = error;
        useNativeSkillsStore.setState({ error: String(error) });
      }
    } while (refreshQueued);

    useNativeSkillsStore.setState({ loading: false });
    if (lastError) throw lastError;
  })().finally(() => {
    refreshPromise = null;
  });

  return refreshPromise;
}

export const useNativeSkillsStore = create<NativeSkillsStore>(() => ({
  descriptors: [],
  loading: false,
  error: null,
  generation: 0,
  refresh: refreshNativeSkills,
  invalidate: (_reason: string) => {
    void refreshNativeSkills(true).catch(() => undefined);
  },
}));

let subscribed = false;
let subscriptionPromise: Promise<void> | null = null;

/** Re-discover native skills in every open window after a managed mutation or
 * an external `.agents/skills` filesystem change. */
export async function subscribeToNativeSkillChanges(): Promise<void> {
  if (!isTauri() || subscribed) return;
  if (!subscriptionPromise) {
    subscriptionPromise = (async () => {
      const ownLabel = getCurrentWindow().label;
      await listen<string>(NATIVE_SKILLS_CHANGED_EVENT, (event) => {
        if (event.payload === ownLabel) return;
        useNativeSkillsStore.getState().invalidate(event.payload);
      });
      subscribed = true;
    })().catch((error) => {
      subscriptionPromise = null;
      throw error;
    });
  }
  await subscriptionPromise;
  await useNativeSkillsStore.getState().refresh().catch(() => undefined);
}
