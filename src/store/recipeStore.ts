import { create } from "zustand";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { errorMessage } from "../lib/errors";

/** Emitted by the backend after a successful `recipes_save`/`recipes_delete`
 * (see src-tauri/src/recipes.rs), with the acting window's label as payload —
 * same cross-window sync mechanism as `promptStore.ts`/`sessionStore.ts`:
 * another open window re-lists on this instead of polling. */
const RECIPES_CHANGED_EVENT = "recipes://changed";

export type RecipeSourceKind = "workspace" | "global";

/** Mirrors `recipes.rs`'s `RecipeTarget` field-for-field (snake_case on the
 * wire — this struct has no `#[serde(rename_all = "camelCase")]`, unlike
 * `PromptEntry`). Exactly one of `provider` (+ `model`), `ollama`, or
 * `local_url` is set — enforced server-side by `RecipeTarget::validate`. */
export interface RecipeTarget {
  provider?: string;
  model?: string;
  ollama?: string;
  local_url?: string;
}

export interface RecipeOutput {
  json: boolean;
}

/** A saved recipe, mirroring `recipes.rs`'s `Recipe` struct field-for-field.
 * `params`' value is `string | null` (`null` = declared with no default,
 * must be supplied via a param override at run time — see
 * `recipes::resolve_param_values`). */
export interface Recipe {
  version: number;
  name: string;
  description?: string;
  target: RecipeTarget;
  workspace?: string;
  permission_mode: string;
  system?: string;
  prompt: string;
  params: Record<string, string | null>;
  max_iterations?: number;
  timeout_seconds?: number;
  output: RecipeOutput;
}

/** One recipe file found on disk — `recipe`/`error` are mutually exclusive,
 * mirroring `recipes.rs`'s `DiscoveredRecipe`. A file that failed to
 * parse/validate is still listed (with `error` set) so the panel can show
 * "this recipe is broken" instead of silently omitting it. */
export interface DiscoveredRecipe {
  path: string;
  source: RecipeSourceKind;
  recipe: Recipe | null;
  error: string | null;
}

export interface RecipeStore {
  recipes: DiscoveredRecipe[];
  loading: boolean;
  /** Last list/save/delete failure, surfaced in the panel instead of
   * silently dropping it — cleared on the next successful call. */
  error: string | null;
  /** Re-lists every recipe visible right now (workspace + global) via
   * `recipes_list`. Safe to call anytime; a no-op outside the Tauri shell. */
  refresh: () => Promise<void>;
  /** Saves `content` (YAML) as the global recipe named `name`, then
   * refreshes. Throws on validation/write failure — callers show the error
   * inline rather than losing the user's edits. */
  save: (name: string, content: string) => Promise<Recipe>;
  /** Deletes the global recipe named `name`, then refreshes. */
  remove: (name: string) => Promise<void>;
  /** Validates `content` without saving — the editor's live-validate
   * affordance. Throws with a user-facing message on anything invalid. */
  validate: (content: string) => Promise<Recipe>;
  /** Reads a recipe's raw (unparsed) YAML/JSON text by name — the Edit
   * action's source for the textarea, since the listed `DiscoveredRecipe`
   * only carries the already-parsed `Recipe`. */
  readRaw: (nameOrPath: string) => Promise<string>;
}

export const useRecipeStore = create<RecipeStore>((set, get) => ({
  recipes: [],
  loading: false,
  error: null,

  refresh: async () => {
    if (!isTauri()) return;
    set({ loading: true });
    try {
      const recipes = await invoke<DiscoveredRecipe[]>("recipes_list");
      set({ recipes, loading: false, error: null });
    } catch (err) {
      set({ loading: false, error: errorMessage(err) });
    }
  },

  save: async (name, content) => {
    const recipe = await invoke<Recipe>("recipes_save", { name, content });
    await get().refresh();
    return recipe;
  },

  remove: async (name) => {
    await invoke("recipes_delete", { name });
    await get().refresh();
  },

  validate: (content) => invoke<Recipe>("recipes_validate", { content }),

  readRaw: (nameOrPath) => invoke<string>("recipes_read_raw", { nameOrPath }),
}));

let subscribed = false;

/** Starts listening for other windows' recipe saves/deletes — idempotent
 * (safe to call from more than one component's mount effect), a no-op
 * outside the Tauri shell. Called once from `App.tsx`'s boot effect. */
export async function subscribeToRecipeChanges(): Promise<void> {
  if (!isTauri() || subscribed) return;
  subscribed = true;
  const ownLabel = getCurrentWindow().label;
  await listen<string>(RECIPES_CHANGED_EVENT, (event) => {
    if (event.payload === ownLabel) return;
    void useRecipeStore.getState().refresh();
  });
}
