/**
 * Execution glue for the Global Command Palette's Quick Actions. Every
 * function here is a thin wrapper around an *existing* Tauri command/store
 * action the rest of the app already uses for the same job — there is no
 * palette-only execution path:
 *
 * - summarize / rewrite / translate / ask model -> `runAgentTurn`, the exact
 *   function the chat composer's Send button calls (same permission
 *   prompts, checkpoints, Stop, and run-ledger evidence as any chat turn).
 * - start workflow -> `runRecipeNow`, the exact function "Run now" in
 *   Settings > Tasks calls.
 * - search knowledge -> `knowledgeV2Store.query`, the same call the
 *   Knowledge panel's "Retrieval inspector" makes, wrapped in a lightweight
 *   durable run so it's auditable/cancellable from Run Center too.
 * - create task -> `recipeStore.save`, the same call the Tasks panel's
 *   recipe editor uses — a task created here is an ordinary recipe, visible
 *   and runnable from Settings > Tasks like any other.
 * - approve pending action -> `permissionStore.respond`, the exact function
 *   the permission modal's own buttons call.
 */
import type { AttachmentRef } from "./agentLoop";
import { runAgentTurn } from "./agentLoop";
import { runRecipeNow } from "./recipeRunner";
import { wrapUntrustedContent } from "./untrustedContent";
import { buildModelTargetInventory, findActiveModelTarget, type ModelTargetSnapshot } from "./modelTargets";
import { beginDurableRun } from "./durableRun";
import { useSessionStore } from "../store/sessionStore";
import { useModelStore } from "../store/modelStore";
import { useWorkspaceStore } from "../store/workspaceStore";
import { usePermissionStore } from "../store/permissionStore";
import { useRecipeStore, type Recipe } from "../store/recipeStore";
import {
  useKnowledgeV2Store,
  DEFAULT_HYBRID_CONFIG,
  type KnowledgeInspectorResponse,
} from "../store/knowledgeV2Store";

export type CapturedContextSource = "clipboard" | "file" | "screenshot" | "snippet";

/** Explicit input captured for the palette to send — never anything read
 * implicitly. `text`/`imageDataUrl` mirror exactly what the confirmation
 * step (`CommandPalette.tsx`) previews before any quick action runs. */
export interface CapturedContext {
  source: CapturedContextSource;
  text: string | null;
  imageDataUrl: string | null;
  /** Source path, when `source === "file"` — shown in the preview, never
   * sent on its own without the file's content/attachment. */
  path?: string | null;
}

export type QuickActionId = "summarize" | "rewrite" | "translate" | "askModel";

/** Pure prompt construction — split out from `runQuickAction` so it's
 * directly unit-testable without touching any store or IPC call. Captured
 * text is always wrapped via `wrapUntrustedContent` (the same boundary used
 * for pasted/transcribed companion context and RAG passages), since it came
 * from outside the app and must never be read as instructions. */
export function buildQuickActionPrompt(
  action: QuickActionId,
  context: CapturedContext | null,
  extra = "",
): string {
  const wrapped = context?.text
    ? wrapUntrustedContent(`command palette ${context.source} capture`, context.text)
    : null;
  const trimmedExtra = extra.trim();
  switch (action) {
    case "summarize":
      return [
        "Summarize the following clearly and concisely, capturing the key points.",
        wrapped ?? "(No captured text was attached — summarize the attached image instead.)",
      ].join("\n\n");
    case "rewrite":
      return [
        "Rewrite the following for clarity and correctness while preserving its meaning and tone. Reply with only the rewritten text.",
        wrapped ?? "(No captured text was attached — there is nothing to rewrite.)",
      ].join("\n\n");
    case "translate": {
      const language = trimmedExtra || "English";
      return [
        `Translate the following into ${language}. Reply with only the translation.`,
        wrapped ?? "(No captured text was attached — there is nothing to translate.)",
      ].join("\n\n");
    }
    case "askModel":
      return [trimmedExtra, wrapped].filter((part): part is string => Boolean(part)).join("\n\n");
    default:
      return trimmedExtra;
  }
}

/** A captured screenshot (or, in principle, an image file) rides along as a
 * vision attachment instead of being inlined as text — same convention the
 * companion overlay's `m7://compose` handling already uses in ChatWindow. */
export function contextAttachments(context: CapturedContext | null): AttachmentRef[] {
  if (!context?.imageDataUrl) return [];
  return [
    {
      path: `palette://${context.source}/${Date.now()}.png`,
      isDir: false,
      kind: "image",
      dataUrl: context.imageDataUrl,
    },
  ];
}

/** Picks the session a quick action should run in: the active session if
 * it isn't already mid-turn, otherwise a fresh one — the same "never
 * interrupt a running turn" rule `runAgentTurn` itself enforces (it throws
 * if a turn is already running for the target session id). */
export function resolveTargetSessionId(): string {
  const state = useSessionStore.getState();
  const active = state.sessions.find((session) => session.id === state.activeSessionId);
  if (active && state.runningTurns[active.id] !== true) return active.id;
  state.newSession();
  return useSessionStore.getState().activeSessionId;
}

export interface QuickActionOutcome {
  sessionId: string;
}

/** Runs a quick action through the exact same `runAgentTurn` the chat
 * composer's Send button calls. */
export async function runQuickAction(
  action: QuickActionId,
  context: CapturedContext | null,
  extra = "",
): Promise<QuickActionOutcome> {
  const prompt = buildQuickActionPrompt(action, context, extra);
  const attachments = contextAttachments(context);
  if (!prompt.trim() && attachments.length === 0) {
    throw new Error("Nothing to send — capture some context or type something first.");
  }
  const sessionId = resolveTargetSessionId();
  await runAgentTurn(sessionId, prompt, attachments);
  return { sessionId };
}

/** Runs a saved recipe ("workflow"/"task") now via `runRecipeNow` —
 * checkpoints, Stop, the permission modal, and run-ledger evidence all come
 * free from it, same as "Run now" in Settings > Tasks. Captured text is
 * passed along as a param override only when the recipe already declares a
 * conventionally-named free-form param (`context`/`input`/`text`/`prompt`)
 * — never invented, never required. */
export async function runStartWorkflow(
  recipe: Recipe,
  context: CapturedContext | null,
): Promise<QuickActionOutcome> {
  const overrides: Record<string, string> = {};
  if (context?.text) {
    const candidateParam = ["context", "input", "text", "prompt"].find((name) => name in recipe.params);
    if (candidateParam) overrides[candidateParam] = context.text;
  }
  const { sessionId, done } = await runRecipeNow(recipe, overrides);
  // Mirrors ScheduledTasksPanel's own "Run now" button: the caller switches
  // to `sessionId` immediately and watches the turn stream in; nothing here
  // needs to await full completion, but an unawaited rejection must still
  // never surface as an unhandled promise rejection.
  void done.catch(() => {});
  return { sessionId };
}

function yamlScalar(value: string): string {
  // A JSON string literal is also a valid YAML flow scalar — the simplest
  // correct way to quote/escape arbitrary text (names, single-line
  // descriptions) without hand-rolling YAML escaping rules.
  return JSON.stringify(value);
}

/** Creates a new recipe ("task") from the palette, saved through the exact
 * same `recipes_save` command Settings > Tasks' recipe editor uses — the
 * result is an ordinary recipe file, immediately visible/runnable/
 * schedulable there, not a separate palette-only record. */
export async function runCreateTask(name: string, prompt: string): Promise<Recipe> {
  const trimmedName = name.trim();
  const trimmedPrompt = prompt.trim();
  if (!trimmedName) throw new Error("Give the task a name.");
  if (!trimmedPrompt) throw new Error("Give the task something to do.");

  const modelState = useModelStore.getState();
  const inventory = buildModelTargetInventory(modelState);
  const active = findActiveModelTarget(inventory, modelState);
  if (!active || active.kind === "local") {
    throw new Error(
      "Saved tasks need an Ollama or cloud-provider model selected — switch off the local runtime first.",
    );
  }
  const targetYaml =
    active.kind === "ollama"
      ? `ollama: ${yamlScalar(active.model)}`
      : `provider: ${yamlScalar(active.providerId)}\n  model: ${yamlScalar(active.model)}`;
  const permissionMode = usePermissionStore.getState().mode;
  // Recipes reject `bypass` outright (see `recipeRunner.ts::assertValidMode`)
  // — falling back to `manual` here means a task created while the app
  // happens to be in bypass mode still saves successfully, just under the
  // safest mode, rather than failing the save entirely.
  const safeMode = permissionMode === "bypass" ? "manual" : permissionMode;

  const yaml = [
    "version: 1",
    `name: ${yamlScalar(trimmedName)}`,
    `description: ${yamlScalar("Created from the Global Command Palette")}`,
    "target:",
    `  ${targetYaml}`,
    `permission_mode: ${safeMode}`,
    "prompt: |",
    ...trimmedPrompt.split("\n").map((line) => `  ${line}`),
    "params: {}",
  ].join("\n");

  return useRecipeStore.getState().save(trimmedName, yaml);
}

export interface SearchKnowledgeOutcome {
  runId: string;
  response: KnowledgeInspectorResponse;
}

/** Runs a knowledge-stack search via the same `knowledge_v2_query` call the
 * Knowledge panel's "Retrieval inspector" makes, wrapped in a lightweight
 * durable run (`kind: "background"`) purely for palette-action evidence and
 * cancellation — the query itself already supports `cancelSearchKnowledge`
 * below via its own `queryId`. */
export async function runSearchKnowledge(
  stackId: string,
  query: string,
): Promise<SearchKnowledgeOutcome> {
  const trimmedQuery = query.trim();
  if (!stackId) throw new Error("Choose a knowledge stack first.");
  if (!trimmedQuery) throw new Error("Type something to search for.");

  const target = currentModelTargetSnapshot();
  if (!target) {
    throw new Error("Select a model before searching knowledge — the search run needs one to record against.");
  }

  const roots = useWorkspaceStore.getState().roots;
  const runId = crypto.randomUUID();
  const recorder = await beginDurableRun({
    runId,
    kind: "background",
    task: `Command palette: search knowledge — ${trimmedQuery}`.slice(0, 4_000),
    target,
    roots,
    permissionMode: usePermissionStore.getState().mode,
    workspaceAccess: "read_only",
    // Declared rather than defaulted to `false`: this run queries a knowledge
    // stack, whose retrieval can reach a cloud embedding provider. Omitting the
    // flag froze a permission the run could then contradict.
    allowNetwork: target.kind === "provider" || (target.kind === "ollama" && target.isCloud === true),
  });
  try {
    const response = await useKnowledgeV2Store
      .getState()
      .query(stackId, trimmedQuery, DEFAULT_HYBRID_CONFIG, [], false, 4_096, runId);
    await recorder?.complete(`${response.search.hits.length} result(s) for "${trimmedQuery}".`);
    return { runId, response };
  } catch (error) {
    await recorder?.fail(error);
    throw error;
  }
}

export function cancelSearchKnowledge(runId: string): Promise<boolean> {
  return useKnowledgeV2Store.getState().cancelQuery(runId);
}

function currentModelTargetSnapshot(): ModelTargetSnapshot | null {
  const modelState = useModelStore.getState();
  const inventory = buildModelTargetInventory(modelState);
  return findActiveModelTarget(inventory, modelState);
}

/** Approves or denies the currently pending permission request through the
 * exact same `permissionStore.respond` the permission modal's own Allow/Deny
 * buttons call — "allow for session" is never offered from the palette
 * (matching the modal's own `run_shell` restriction, but here for every
 * tool: a one-line palette approval is not the place for an unattended
 * multi-call grant). */
export async function runApprovePending(allow: boolean): Promise<void> {
  const pending = usePermissionStore.getState().pending;
  if (!pending) throw new Error("There is no pending permission request to decide.");
  await usePermissionStore.getState().respond(allow, false);
}
