/**
 * "Run now" for a saved recipe (design doc: docs/roadmap/p3-scheduled-automation.md,
 * slice 2) — the desktop-app equivalent of `monkey-cli task run`. Creates a
 * tagged session, applies the recipe's target, and calls `runAgentTurn`
 * unchanged: checkpoints, Stop, and the permission modal all come free since
 * this is an ordinary turn, not a special code path.
 */
import { invoke } from "@tauri-apps/api/core";

import { useModelStore } from "../store/modelStore";
import { useSessionStore } from "../store/sessionStore";
import { VALID_PERMISSION_MODES, type PermissionMode } from "../store/permissionStore";
import type { Recipe } from "../store/recipeStore";
import { formatRecipeNotice, runAgentTurn } from "./agentLoop";

/**
 * Applies a recipe's `target` as the app's active chat target — the GUI
 * equivalent of `monkey-cli task run`'s `resolve_chat_target`. Supports
 * `provider` and `ollama` targets, the same two the desktop app's
 * `ModelSwitcher` can select. A `local_url` target (an arbitrary
 * OpenAI-compatible server) has no GUI equivalent: `ChatTarget`'s `"local"`
 * kind always means the app's own managed llama-server, never a
 * caller-supplied URL — rejected here with a message pointing at `monkey-cli
 * task run` instead of silently running against the wrong server.
 */
function applyRecipeTarget(recipe: Recipe): void {
  const target = recipe.target;
  if (target.provider) {
    if (!target.model) {
      throw new Error(`Recipe '${recipe.name}' target has 'provider' but no 'model'.`);
    }
    useModelStore.getState().useProviderModel(target.provider, target.model);
    return;
  }
  if (target.ollama) {
    useModelStore.getState().useOllamaModel(target.ollama);
    return;
  }
  if (target.local_url) {
    throw new Error(
      `Recipe '${recipe.name}' targets a custom local URL (${target.local_url}), which the desktop app has no equivalent for — run it with \`monkey-cli task run ${recipe.name}\` instead.`,
    );
  }
  throw new Error(`Recipe '${recipe.name}' has no valid target.`);
}

/** Validates a recipe's `permission_mode` (or a scheduler override) against
 * the same source of truth `permissionStore.ts` itself uses, narrowing the
 * type on success — then separately rejects `bypass`, exactly like
 * `recipes.rs`'s `validate_recipe` does for the recipe's own field. That
 * Rust check guards `recipe.permission_mode` at parse time, but
 * `permissionModeOverride` (the scheduler's escape hatch, set outside any
 * recipe file) never passes through `validate_recipe` at all — this is the
 * only gate it gets, so it has to hold the same line: a recipe can run
 * fully unattended, and `bypass` would auto-approve every tool, `run_shell`
 * included, with nobody present to catch it. */
function assertValidMode(mode: string): asserts mode is PermissionMode {
  if (!VALID_PERMISSION_MODES.includes(mode as PermissionMode)) {
    throw new Error(`Recipe permission_mode '${mode}' is not a valid mode (expected one of ${VALID_PERMISSION_MODES.join(", ")}).`);
  }
  if (mode === "bypass") {
    throw new Error(
      "Recipe permission_mode 'bypass' is not allowed — recipes can run unattended, and bypass auto-approves every tool (including run_shell) with nobody present to catch it.",
    );
  }
}

/** What `runRecipeNow` hands back: `sessionId` is available immediately (so
 * the UI can switch to it and watch the turn stream in, same as any other
 * new session) while `done` resolves only once the underlying turn — and
 * this run's turn-scoped permission-mode override — actually clear. UI
 * callers ignore `done`; `scheduler.ts` awaits it before recording the run's
 * outcome. */
export interface RecipeRunHandle {
  sessionId: string;
  done: Promise<void>;
}

/**
 * Runs `recipe` now: renders its `{{param}}` placeholders server-side (via
 * `recipes_render` — the same substitution `recipes::render_recipe` does for
 * `monkey-cli task run`, so there is exactly one implementation, not two
 * independently maintained ones), applies its target, and creates a new
 * tagged session with a `[Recipe]` notice marking where it came from before
 * starting the turn under its own permission mode (`permissionModeOverride`,
 * if given — the scheduler's use — otherwise the recipe's own
 * `permission_mode`).
 *
 * The permission mode is applied via a turn-scoped override
 * (`permissions.rs`'s `set_permission_mode_for_turn`/
 * `clear_permission_mode_for_turn`, Phase 2b) rather than the app's single
 * global mode: a turn id is minted here (instead of letting `runAgentTurn`
 * generate its own) specifically so the override can be set *before* the
 * turn's first tool call and cleared once `done` settles, with no window
 * where this run's mode leaks into a concurrent turn in a different
 * split-pane session, and no global mode left changed behind it. `recipe`
 * validation already rejects `permission_mode: bypass` (see
 * `recipes.rs`'s `validate_recipe`) — this scheduled/headless path never
 * short-circuits every prompt with nobody present to catch it.
 */
export async function runRecipeNow(
  recipe: Recipe,
  paramOverrides: Record<string, string> = {},
  permissionModeOverride?: string,
): Promise<RecipeRunHandle> {
  // Every error path in this function is a rejected promise, never a
  // synchronous throw — callers that don't wrap the call in try/catch (e.g.
  // a bare `.catch()` chain) still see every failure.
  const mode = permissionModeOverride ?? recipe.permission_mode;
  assertValidMode(mode);

  const rendered = await invoke<{ prompt: string; system: string | null }>("recipes_render", {
    nameOrPath: recipe.name,
    overrides: paramOverrides,
  });

  applyRecipeTarget(recipe);

  const turnId = crypto.randomUUID();
  await invoke("set_permission_mode_for_turn", { turnId, mode });

  useSessionStore.getState().newSession();
  const sessionId = useSessionStore.getState().activeSessionId;
  useSessionStore.getState().renameSession(sessionId, recipe.name);
  useSessionStore.getState().addMessage(sessionId, {
    role: "system",
    content: formatRecipeNotice({ name: recipe.name }),
  });

  // A recipe's own `system` (if any) is appended the same way every other
  // system-prompt-extension in this codebase is: never replacing the base
  // agent prompt `currentSystemPrompt` builds fresh every turn, just adding
  // to it. There's no per-session "extra system text" slot to set ahead of
  // time, so it's folded into the rendered prompt itself as a preamble
  // instead — the model sees it as part of this turn's own instructions,
  // which is an equally valid way to honor a recipe's `system` field for a
  // single one-shot run (unlike a persona, this never needs to persist
  // across further turns in the tagged session).
  const promptWithSystem = rendered.system ? `${rendered.system}\n\n${rendered.prompt}` : rendered.prompt;

  const done = runAgentTurn(sessionId, promptWithSystem, [], undefined, turnId).finally(() => {
    void invoke("clear_permission_mode_for_turn", { turnId }).catch(() => {});
  });

  return { sessionId, done };
}
