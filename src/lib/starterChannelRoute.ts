/**
 * The first route a messaging account needs, created without an operator
 * writing a recipe file.
 *
 * An account with a credential accepts messages the moment it connects, and a
 * route is what decides which task those messages run as. Without one every
 * message is recorded and then fails — visible only as one line of a channel's
 * activity list, which reads as "the app is broken" rather than as "nothing is
 * configured". So the starter pair is made here: an ordinary task, saved
 * through the same `recipes_save` the Tasks panel uses, and a global route
 * pointing at it.
 *
 * Nothing about it is special-cased afterwards. It is editable, renameable and
 * deletable in Settings > Tasks like any other task, and the route is editable
 * in Channels like any other route.
 */
import { channelsAddRoute } from "./channelsClient";
import { useRecipeStore, type DiscoveredRecipe } from "../store/recipeStore";

/** The task the starter route names. A fixed name rather than "whatever task
 * is saved first": adopting one the operator wrote for another job is a worse
 * surprise than creating one that says what it is. */
export const STARTER_RECIPE = "channel-chat";

/** The whole task: answer the message. `message` is declared because the route
 * supplies it, and an undeclared param is refused outright at run time
 * (`recipes::resolve_param_values`). */
const STARTER_PROMPT = "{{message}}";

/** Why this has to be said out loud: a run answers a channel by *calling*
 * `send_message` (see `daemon/channel_tool.rs`). Text the model merely writes
 * is the run's own output — recorded, and delivered to nobody. A model that is
 * not told this answers into the void, and the run is marked succeeded, which
 * is the most confusing failure this path has. */
const STARTER_SYSTEM = [
  "You are answering a person who messaged this machine on a messaging channel.",
  "Send your answer by calling the send_message tool. Text you write outside a tool call is NOT delivered to them.",
  "Reply once, briefly, in the language they wrote in.",
].join("\n");

function hasStarter(recipes: DiscoveredRecipe[]): boolean {
  return recipes.some((entry) => entry.recipe?.name === STARTER_RECIPE && !entry.error);
}

/** The starter task's name, saving it first if this machine does not have it.
 *
 * `paletteActions` is imported at call time: it pulls in the agent loop and
 * the recipe runner, and a settings panel that merely renders a button has no
 * reason to carry either until the button is pressed. */
export async function ensureStarterRecipe(): Promise<string> {
  const store = useRecipeStore.getState();
  // The list may simply not have been fetched yet; asking is cheaper than
  // saving a second copy over the operator's own edits.
  if (!hasStarter(store.recipes)) await store.refresh();
  if (hasStarter(useRecipeStore.getState().recipes)) return STARTER_RECIPE;

  const { runCreateTask } = await import("./paletteActions");
  const created = await runCreateTask(STARTER_RECIPE, STARTER_PROMPT, {
    params: { message: "" },
    description: "Answer a message that arrived on a messaging channel.",
    system: STARTER_SYSTEM,
    // Read-only, and deliberately not the app's current mode: this task runs
    // unattended, on text written by whoever messaged the account, so it must
    // not inherit an `auto` or `acceptEdits` a person chose for their own
    // supervised chat. Replying is unaffected — the route's own grant carries
    // that, not the permission mode.
    permissionMode: "plan",
  });
  return created.name;
}

/** Creates the starter task if it is missing, then points a global route —
 * every account, every conversation — at it. */
export async function ensureStarterChannelRoute(): Promise<void> {
  await channelsAddRoute(await ensureStarterRecipe(), {});
}
