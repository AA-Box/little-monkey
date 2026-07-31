import { create } from "zustand";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

import type { Recipe } from "./recipeStore";
import { runRecipeNow } from "../lib/recipeRunner";
import { errorMessage } from "../lib/errors";

/** Emitted after a successful `local_apps_publish`/`local_apps_unpublish`
 * (see `src-tauri/src/local_apps.rs`), with the acting window's label as
 * payload — same cross-window sync convention as `recipes.rs`'s
 * `RECIPES_CHANGED_EVENT`. */
const LOCAL_APPS_CHANGED_EVENT = "local-apps://changed";

/** Emitted by the local API server's `POST /v1/local-apps/{id}/run` route
 * once a run request has cleared scope/binding checks, param validation, and
 * human approval — see `server.rs`'s `handle_local_app_run`. */
const LOCAL_APP_RUN_REQUESTED_EVENT = "local-apps://run-requested";

/** Mirrors `local_apps.rs`'s `LocalAppTemplate` enum exactly
 * (`#[serde(rename_all = "snake_case")]`). */
export type LocalAppTemplate = "form" | "dashboard" | "approval_page" | "report_generator" | "chat_widget";

export const LOCAL_APP_TEMPLATES: LocalAppTemplate[] = [
  "form",
  "dashboard",
  "approval_page",
  "report_generator",
  "chat_widget",
];

/** Mirrors `local_apps.rs`'s `LocalAppDefinition` struct field-for-field. */
export interface LocalAppDefinition {
  id: string;
  name: string;
  recipe_name: string;
  template: LocalAppTemplate;
  param_bindings: Record<string, string>;
  scoped_token_id: string;
  created_at: number;
  enabled: boolean;
}

/** Payload carried by `local-apps://run-requested` — mirrors
 * `local_apps.rs`'s `LocalAppRunRequestedPayload`. */
export interface LocalAppRunRequestedPayload {
  app_id: string;
  recipe_name: string;
  params: Record<string, string>;
}

export interface LocalAppsStore {
  apps: LocalAppDefinition[];
  loading: boolean;
  /** Last list/publish/unpublish failure, surfaced by the panel instead of
   * silently dropping it — cleared on the next successful call. */
  error: string | null;
  refresh: () => Promise<void>;
  publish: (
    recipeName: string,
    template: LocalAppTemplate,
    paramBindings: Record<string, string>,
  ) => Promise<LocalAppDefinition>;
  unpublish: (id: string) => Promise<void>;
  /** Resolves the local URL a published app is served at — the local API
   * server must actually be running on that port for the link to load. */
  open: (id: string) => Promise<string>;
}

export const useLocalAppsStore = create<LocalAppsStore>((set, get) => ({
  apps: [],
  loading: false,
  error: null,

  refresh: async () => {
    if (!isTauri()) return;
    set({ loading: true });
    try {
      const apps = await invoke<LocalAppDefinition[]>("local_apps_list");
      set({ apps, loading: false, error: null });
    } catch (err) {
      set({ loading: false, error: errorMessage(err) });
    }
  },

  publish: async (recipeName, template, paramBindings) => {
    const definition = await invoke<LocalAppDefinition>("local_apps_publish", {
      recipeName,
      template,
      paramBindings,
    });
    await get().refresh();
    return definition;
  },

  unpublish: async (id) => {
    await invoke("local_apps_unpublish", { id });
    await get().refresh();
  },

  open: (id) => invoke<string>("local_apps_open", { id }),
}));

let subscribedToChanges = false;

/** Starts listening for other windows' publish/unpublish calls — idempotent,
 * a no-op outside the Tauri shell. Called once from `App.tsx`'s boot
 * effect, mirroring `recipeStore.ts`'s `subscribeToRecipeChanges`. */
export async function subscribeToLocalAppsChanges(): Promise<void> {
  if (!isTauri() || subscribedToChanges) return;
  subscribedToChanges = true;
  const ownLabel = getCurrentWindow().label;
  await listen<string>(LOCAL_APPS_CHANGED_EVENT, (event) => {
    if (event.payload === ownLabel) return;
    void useLocalAppsStore.getState().refresh();
  });
}

let subscribedToRunRequests = false;

/** Starts listening for `local-apps://run-requested` — the local API
 * server's bridge from an external HTTP caller (a published app's static
 * page) into the desktop app's own agent-turn loop. Every emitted request
 * has already cleared scope/binding checks, param validation, and a human
 * approval prompt on the Rust side (`server.rs`'s `handle_local_app_run`);
 * this listener's only job is to resolve the named recipe and hand it to
 * `recipeRunner.ts`'s `runRecipeNow`, tagged with the app id, so the run
 * gets an ordinary session, turn, and Run Capsule like any other recipe
 * run. Idempotent and main-window-only, called from `App.tsx`'s boot
 * effect — mirrors that file's existing `onRunCancellationRequested`
 * listener (only one window should ever act on a given daemon-wide event). */
export async function subscribeToLocalAppRunRequests(): Promise<void> {
  if (!isTauri() || subscribedToRunRequests) return;
  subscribedToRunRequests = true;
  await listen<LocalAppRunRequestedPayload>(LOCAL_APP_RUN_REQUESTED_EVENT, (event) => {
    void (async () => {
      try {
        const recipe = await invoke<Recipe>("recipes_read", { nameOrPath: event.payload.recipe_name });
        await runRecipeNow(recipe, event.payload.params, undefined, event.payload.app_id);
      } catch (err) {
        console.error("Failed to run a Local App's recipe", err);
      }
    })();
  });
}
