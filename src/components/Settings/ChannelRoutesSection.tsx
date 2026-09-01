import { useCallback, useEffect, useMemo, useState } from "react";
import { Loader2, Pencil, Plus, Power, Trash2, X } from "lucide-react";
import {
  type ChannelAccount,
  type ChannelRoute,
  type ChannelRouteScope,
  type RouteOptions,
  type SessionScope,
  channelsAddRoute,
  channelsEnableRoute,
  channelsEvents,
  channelsRemoveRoute,
  channelsRoutes,
  channelsUpdateRoute,
  routeSpecificity,
} from "../../lib/channelsClient";
import { Button } from "../ui";
import { errorMessage } from "../../lib/errors";
import { useT } from "../../lib/i18n";
import { useRecipeStore, type Recipe, type RecipeTarget } from "../../store/recipeStore";
import { useModelStore } from "../../store/modelStore";
import {
  buildRecipeTargetOptions,
  isTargetAvailable,
  recipeTargetKey,
  recipeTargetLabel,
  type RecipeTargetOption,
} from "../../lib/channelRouteModel";
import { ensureStarterChannelRoute, ensureStarterRecipe } from "../../lib/starterChannelRoute";

const INPUT =
  "w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent";

/**
 * Route management: the whole specificity ladder, editable.
 *
 * The daemon's ladder runs from a single sender in a single thread down to one
 * global default, and until now only two of those rungs could be configured
 * outside the terminal. Every field here maps to one the daemon already
 * understands; nothing is computed into a routing rule locally. Whether a
 * scope is legal, and whether two routes would tie, is the daemon's answer —
 * this form sends what the operator chose and shows what came back.
 *
 * Ids are offered from what this account has actually seen rather than asked
 * for from memory: a Telegram group id is a negative integer nobody recalls,
 * and typing it wrong produces a route that silently matches nothing.
 */

/** Which rung the form is describing. One control, so an operator picks a rung
 * rather than discovering which field combinations the daemon rejects. */
type ScopeLevel = "global" | "provider" | "account" | "conversation" | "thread" | "sender";

/** One recipe parameter row in the editor. Kept as an ordered list rather
 * than an object so a row being renamed does not jump around the form. */
export interface ParamRow {
  name: string;
  value: string;
}

interface RouteDraft {
  /** The route being edited, or null while adding. */
  routeId: string | null;
  recipe: string;
  level: ScopeLevel;
  kind: string;
  accountId: string;
  conversationId: string;
  threadId: string;
  senderId: string;
  repository: string;
  params: ParamRow[];
  sessionScope: SessionScope;
  priority: string;
  reply: boolean;
  enabled: boolean;
}

const EMPTY_DRAFT: RouteDraft = {
  routeId: null,
  recipe: "",
  level: "global",
  kind: "telegram",
  accountId: "",
  conversationId: "",
  threadId: "",
  senderId: "",
  repository: "",
  params: [],
  sessionScope: "thread",
  priority: "",
  reply: true,
  enabled: true,
};

/** The rung an existing route's scope sits on, so editing it opens on the
 * right one. */
function levelOf(scope: ChannelRouteScope): ScopeLevel {
  if (scope.sender_id) return "sender";
  if (scope.thread_id) return "thread";
  if (scope.conversation_id) return "conversation";
  if (scope.account_id) return "account";
  if (scope.kind) return "provider";
  return "global";
}

export function draftFrom(route: ChannelRoute): RouteDraft {
  return {
    routeId: route.route_id,
    recipe: route.target.recipe,
    level: levelOf(route.scope),
    kind: route.scope.kind ?? "telegram",
    accountId: route.scope.account_id ?? "",
    conversationId: route.scope.conversation_id ?? "",
    threadId: route.scope.thread_id ?? "",
    senderId: route.scope.sender_id ?? "",
    repository: route.target.repository ?? "",
    // Every stored parameter becomes a row, so saving an unrelated edit sends
    // them all back — the daemon replaces the target wholesale, and a param
    // the form forgot to carry would simply be gone.
    params: Object.entries(route.target.params ?? {}).map(([name, value]) => ({ name, value })),
    sessionScope: route.target.session_scope,
    priority: route.target.priority === 0 ? "" : String(route.target.priority),
    reply: route.target.reply_to_conversation,
    enabled: route.enabled,
  };
}

/** Why the parameter rows cannot be saved yet, or null when they can.
 * Mirrors nothing: the daemon rejects an empty name too — this only says so
 * while the operator is still looking at the row. A duplicate name would
 * silently collapse into one entry on the daemon side, so it is refused here
 * where both rows are still visible. */
export function paramsProblem(
  params: ParamRow[],
): "empty_name" | "invalid_name" | "duplicate_name" | null {
  const names = params.map((row) => row.name.trim());
  if (names.some((name) => name.length === 0)) return "empty_name";
  // `=` is the wire separator between name and value; a name carrying one
  // would silently become a different parameter with a longer value.
  if (names.some((name) => name.includes("="))) return "invalid_name";
  if (new Set(names).size !== names.length) return "duplicate_name";
  return null;
}

/** Which scope fields a rung carries. A field the rung does not use is not
 * sent at all, so a leftover conversation id from an earlier edit cannot turn
 * an account route into a conversation one. */
function scopeFields(level: ScopeLevel): {
  account: boolean;
  conversation: boolean;
  thread: boolean;
  sender: boolean;
  kind: boolean;
} {
  return {
    kind: level === "provider",
    account: level !== "global" && level !== "provider",
    conversation: level === "conversation" || level === "thread" || level === "sender",
    thread: level === "thread" || level === "sender",
    sender: level === "sender",
  };
}

/** The daemon's route options for this draft. Empty strings become absent
 * fields rather than empty ids. */
export function draftOptions(draft: RouteDraft): RouteOptions {
  const uses = scopeFields(draft.level);
  const value = (raw: string) => {
    const trimmed = raw.trim();
    return trimmed.length > 0 ? trimmed : null;
  };
  const priority = Number(draft.priority.trim());
  return {
    kind: uses.kind ? draft.kind : null,
    account_id: uses.account ? value(draft.accountId) : null,
    conversation_id: uses.conversation ? value(draft.conversationId) : null,
    thread_id: uses.thread ? value(draft.threadId) : null,
    sender_id: uses.sender ? value(draft.senderId) : null,
    repository: value(draft.repository),
    // The daemon's own `--param name=value` shape, one entry per row. An
    // empty value is legal (the daemon stores it); an empty name is refused
    // before this is ever built.
    params: draft.params.map((row) => `${row.name.trim()}=${row.value}`),
    session_scope: draft.sessionScope,
    priority: draft.priority.trim().length > 0 && Number.isFinite(priority) ? priority : null,
    reply: draft.reply,
    enabled: draft.enabled,
  };
}

/** Whether the draft is still missing something the daemon needs.
 *
 * Every id the chosen rung carries must be present: the daemon refuses a
 * scope missing one, and refusing here says so while the operator is still
 * looking at the field. It is deliberately the *same* list as
 * `scopeFields` — a rung that renders a field but does not require it is how
 * a Thread route silently saves as a Conversation route, and a Sender route
 * as one that follows its sender into every thread. */
export function draftIncomplete(draft: RouteDraft): boolean {
  const uses = scopeFields(draft.level);
  return (
    draft.recipe.trim().length === 0 ||
    paramsProblem(draft.params) !== null ||
    (uses.account && draft.accountId.trim().length === 0) ||
    (uses.conversation && draft.conversationId.trim().length === 0) ||
    (uses.thread && draft.threadId.trim().length === 0) ||
    (uses.sender && draft.senderId.trim().length === 0)
  );
}

/**
 * The model a route's task answers on, shown and changeable in place.
 *
 * The model is not a property of the route: a route names a *task*, and the
 * model lives in that task's own `target:` — which is exactly what the daemon
 * reads when a message arrives (`daemon::freeze_execution_for`). So this reads
 * the task's target and writes the task's target, and nothing here invents a
 * per-route model that the runner would never look at.
 *
 * The consequence, said out loud rather than discovered: two routes naming one
 * task share one model.
 */
export function RouteModelPicker({
  recipeName,
  recipe,
  options,
  sharedWith,
  busy,
  onPick,
}: {
  recipeName: string;
  /** The saved task, or null when the route names one that is not on disk. */
  recipe: Recipe | null;
  options: readonly RecipeTargetOption[];
  /** How many routes name this same task, this one included. */
  sharedWith: number;
  busy: boolean;
  onPick: (recipeName: string, target: RecipeTarget) => void;
}) {
  const { t } = useT();
  const target = recipe?.target ?? null;
  const savedKey = recipeTargetKey(target);
  const available = isTargetAvailable(target, options);
  const groups = useMemo(() => {
    const byGroup = new Map<string, RecipeTargetOption[]>();
    for (const option of options) {
      const bucket = byGroup.get(option.group);
      if (bucket) bucket.push(option);
      else byGroup.set(option.group, [option]);
    }
    return [...byGroup];
  }, [options]);

  if (!recipe) {
    return <span className="block text-xs text-warning">{t("ChannelsPanel.routeModelUnknownRecipe")}</span>;
  }

  return (
    <div className="mt-1">
      <label className="flex flex-wrap items-center gap-2 text-xs text-muted">
        {t("ChannelsPanel.routeModel")}
        <select
          className="min-w-0 max-w-full rounded-md border border-border bg-background px-2 py-1 text-xs text-foreground outline-none focus:ring-1 focus:ring-accent"
          disabled={busy}
          value={savedKey ?? ""}
          onChange={(event) => {
            const picked = options.find((option) => option.key === event.target.value);
            // The saved-but-unavailable entry and the "no model set" entry are
            // not choices: neither maps to an option, and re-selecting either
            // must not write anything.
            if (picked) onPick(recipeName, picked.target);
          }}
        >
          {/* A task whose target this machine cannot currently offer still
              shows the target it actually has. Replacing it with "unknown", or
              quietly snapping the control to the first available model, would
              hide the one thing this row exists to report — and merely opening
              settings must not change what a route answers on. */}
          {savedKey === null && <option value="">{t("ChannelsPanel.routeModelUnset")}</option>}
          {savedKey !== null && !available && (
            <option value={savedKey}>
              {t("ChannelsPanel.routeModelUnavailableOption", { label: recipeTargetLabel(target) })}
            </option>
          )}
          {groups.map(([group, entries]) => (
            <optgroup key={group} label={group}>
              {entries.map((option) => (
                <option key={option.key} value={option.key}>
                  {option.displayName}
                </option>
              ))}
            </optgroup>
          ))}
        </select>
        {busy && <Loader2 size={12} className="animate-spin" />}
      </label>
      {savedKey !== null && !available && (
        <span className="mt-1 block text-xs text-warning">
          {t("ChannelsPanel.routeModelUnavailable")}
        </span>
      )}
      {options.length === 0 && (
        <span className="mt-1 block text-xs text-faint">{t("ChannelsPanel.routeModelEmpty")}</span>
      )}
      {sharedWith > 1 && (
        <span className="mt-1 block text-xs text-faint">
          {t("ChannelsPanel.routeModelShared", { count: String(sharedWith) })}
        </span>
      )}
    </div>
  );
}

/** A one-line description of what a route matches, in the operator's terms. */
function scopeSummary(scope: ChannelRouteScope, accounts: ChannelAccount[]): string {
  const parts: string[] = [];
  if (scope.kind) parts.push(scope.kind);
  if (scope.account_id) {
    const account = accounts.find((entry) => entry.account_id === scope.account_id);
    parts.push(account?.label || scope.account_id);
  }
  if (scope.conversation_id) parts.push(scope.conversation_id);
  if (scope.thread_id) parts.push(`# ${scope.thread_id}`);
  if (scope.sender_id) parts.push(`@ ${scope.sender_id}`);
  return parts.join(" · ");
}

export function ChannelRoutesSection({
  accounts,
  onChanged,
}: {
  accounts: ChannelAccount[];
  onChanged?: () => void;
}) {
  const { t } = useT();
  const [routes, setRoutes] = useState<ChannelRoute[] | null>(null);
  const [draft, setDraft] = useState<RouteDraft | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** Ids this account has actually been seen using, offered as suggestions. */
  const [seen, setSeen] = useState<{ conversations: string[]; threads: string[]; senders: string[] }>({
    conversations: [],
    threads: [],
    senders: [],
  });

  // The tasks a route may name. Offered as a list rather than typed: the
  // daemon resolves a route's recipe by name at message time, so a typo is
  // not refused here — it is a message that arrives, fails, and is only
  // explicable from the event log.
  const recipes = useRecipeStore((state) => state.recipes);
  const refreshRecipes = useRecipeStore((state) => state.refresh);
  const setRecipeTarget = useRecipeStore((state) => state.setTarget);

  // Everything this machine could answer a channel message on. The same
  // inventory the Models, Ollama and provider panels show — read from the same
  // store rather than fetched again here, and refreshed on mount because a
  // settings screen may well be the first thing opened after a restart.
  const installed = useModelStore((state) => state.installed);
  const ollamaModels = useModelStore((state) => state.ollamaModels);
  const ollamaReachable = useModelStore((state) => state.ollamaReachable);
  const providers = useModelStore((state) => state.providers);
  const providerModels = useModelStore((state) => state.providerModels);

  const load = useCallback(async () => {
    try {
      const listed = await channelsRoutes();
      setRoutes(listed.routes);
    } catch (reason) {
      setError(errorMessage(reason));
      setRoutes([]);
    }
  }, []);

  useEffect(() => {
    void load();
    void refreshRecipes();
    const models = useModelStore.getState();
    // Failures are deliberately swallowed: a machine with no Ollama daemon and
    // no provider keys is a normal machine, and its route list must still
    // render. The picker simply offers what did answer.
    void models.refresh().catch(() => {});
    void models.refreshOllama().catch(() => {});
    // Re-hydrates each connected provider's model list too — see
    // `modelStore.refreshProviders`.
    void models.refreshProviders().catch(() => {});
  }, [load, refreshRecipes]);

  const recipeNames = useMemo(
    () => [
      ...new Set(
        recipes.flatMap((entry) => (entry.recipe && !entry.error ? [entry.recipe.name] : [])),
      ),
    ].sort(),
    [recipes],
  );

  /** Task name -> the saved task, so a route row can read its model. */
  const recipesByName = useMemo(() => {
    const byName = new Map<string, Recipe>();
    for (const entry of recipes) {
      if (entry.recipe && !entry.error) byName.set(entry.recipe.name, entry.recipe);
    }
    return byName;
  }, [recipes]);

  const modelOptions = useMemo(
    () =>
      buildRecipeTargetOptions({
        installed,
        ollamaModels,
        ollamaReachable,
        providers,
        providerModels,
      }),
    [installed, ollamaModels, ollamaReachable, providers, providerModels],
  );


  // Recent activity for whichever account the draft names: the observed
  // conversation, thread and sender ids an operator can pick from instead of
  // copying them out of a provider's UI.
  const scopeAccount = draft && scopeFields(draft.level).account ? draft.accountId : "";
  useEffect(() => {
    if (!scopeAccount) {
      setSeen({ conversations: [], threads: [], senders: [] });
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const recent = await channelsEvents(scopeAccount, 50);
        if (cancelled) return;
        const unique = (values: (string | null)[]) =>
          [...new Set(values.filter((value): value is string => !!value))];
        setSeen({
          conversations: unique(recent.events.map((event) => event.conversation_id)),
          threads: unique(recent.events.map((event) => event.thread_id)),
          senders: unique(recent.events.map((event) => event.sender_id)),
        });
      } catch {
        // Suggestions are a convenience; an account with no readable history
        // still gets the manual fields.
        if (!cancelled) setSeen({ conversations: [], threads: [], senders: [] });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [scopeAccount]);

  const run = useCallback(
    async (key: string, action: () => Promise<unknown>) => {
      setBusy(key);
      setError(null);
      try {
        await action();
        await load();
        onChanged?.();
        return true;
      } catch (reason) {
        setError(errorMessage(reason));
        return false;
      } finally {
        setBusy(null);
      }
    },
    [load, onChanged],
  );

  // The first route is *not* created here. Adding an account already creates
  // one, in `channels_cli::report_starter_route`, which makes it only when the
  // machine has no route at all. This screen creating a second one raced that:
  // both wrote a global-default scope, and the loser came back as "Route
  // '<id>' already owns this scope" on a screen the operator had only opened
  // to look at. The empty state's button is the same call, kept for a machine
  // whose accounts predate that guarantee — a person asking for it cannot race
  // themselves.

  const ordered = useMemo(
    () =>
      [...(routes ?? [])].sort((left, right) => {
        const rank = (route: ChannelRoute) =>
          ["sender", "thread", "conversation", "account", "channel_default", "global_default"].indexOf(
            routeSpecificity(route.scope),
          );
        return rank(left) - rank(right) || left.route_id.localeCompare(right.route_id);
      }),
    [routes],
  );

  /** How many routes name each task, so a row can say when a model change
   * moves more than the route the operator is looking at. */
  const routesPerRecipe = useMemo(() => {
    const counts = new Map<string, number>();
    for (const route of routes ?? []) {
      counts.set(route.target.recipe, (counts.get(route.target.recipe) ?? 0) + 1);
    }
    return counts;
  }, [routes]);

  /** Writes the picked model into the task's own `target:` and re-reads it.
   *
   * Nothing is mirrored locally: the select's value is derived from the store's
   * copy of the task, which is only replaced once the backend has written and
   * re-parsed the file. A failed write therefore leaves the control showing the
   * model the runner will really use, and puts the reason in the error line. */
  const pickModel = useCallback(
    (recipeName: string, target: RecipeTarget) => {
      void run(`model-${recipeName}`, () => setRecipeTarget(recipeName, target));
    },
    [run, setRecipeTarget],
  );

  const uses = draft ? scopeFields(draft.level) : null;
  const paramError = draft ? paramsProblem(draft.params) : null;
  const incomplete = draft !== null && draftIncomplete(draft);

  return (
    <section className="rounded-lg border border-border bg-surface p-4">
      <div className="flex items-center justify-between gap-3">
        <h4 className="text-sm font-semibold">{t("ChannelsPanel.routes")}</h4>
        {draft === null && (
          <Button size="sm" onClick={() => setDraft({ ...EMPTY_DRAFT })}>
            <Plus size={14} />
            {t("ChannelsPanel.addRoute")}
          </Button>
        )}
      </div>
      <p className="mt-1 text-xs text-muted">{t("ChannelsPanel.routesIntro")}</p>
      <p className="mt-1 text-xs text-faint">{t("ChannelsPanel.routesLadder")}</p>

      {error && (
        <p className="mt-2 rounded-md border border-danger/40 bg-danger/10 p-2 text-xs text-danger">{error}</p>
      )}

      {routes === null ? (
        <p className="mt-3 flex items-center gap-2 text-xs text-muted">
          <Loader2 size={14} className="animate-spin" />
          {t("ChannelsPanel.loading")}
        </p>
      ) : ordered.length === 0 ? (
        <div className="mt-3 flex flex-col items-start gap-2">
          <p className="text-xs text-muted">{t("ChannelsPanel.noRoutesYet")}</p>
          <Button
            size="sm"
            disabled={busy !== null}
            onClick={() => void run("starter", ensureStarterChannelRoute)}
          >
            {busy === "starter" ? <Loader2 size={14} className="animate-spin" /> : <Plus size={14} />}
            {t("ChannelsPanel.starterRoute")}
          </Button>
          <span className="text-xs text-faint">{t("ChannelsPanel.starterRouteHint")}</span>
        </div>
      ) : (
        <ul className="mt-3 flex flex-col gap-2">
          {ordered.map((route) => (
            <li
              key={route.route_id}
              className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-border bg-background px-3 py-2"
            >
              <span className="min-w-0">
                <span className="text-sm font-medium">{route.target.recipe}</span>
                <span className="ml-2 rounded border border-border px-1 text-[10px] uppercase text-faint">
                  {t(`ChannelsPanel.specificity_${routeSpecificity(route.scope)}`)}
                </span>
                {!route.enabled && (
                  <span className="ml-2 text-xs text-warning">{t("ChannelsPanel.routeDisabled")}</span>
                )}
                <span className="block truncate text-xs text-muted">
                  {scopeSummary(route.scope, accounts) || t("ChannelsPanel.scopeGlobal")}
                </span>
                <RouteModelPicker
                  recipeName={route.target.recipe}
                  recipe={recipesByName.get(route.target.recipe) ?? null}
                  options={modelOptions}
                  sharedWith={routesPerRecipe.get(route.target.recipe) ?? 1}
                  busy={busy === `model-${route.target.recipe}`}
                  onPick={pickModel}
                />
              </span>
              <span className="flex gap-1">
                <Button size="sm" disabled={busy !== null} onClick={() => setDraft(draftFrom(route))}>
                  <Pencil size={12} />
                  {t("ChannelsPanel.editRoute")}
                </Button>
                <Button
                  size="sm"
                  disabled={busy !== null}
                  onClick={() =>
                    void run(`enable-${route.route_id}`, () =>
                      channelsEnableRoute(route.route_id, !route.enabled),
                    )
                  }
                >
                  <Power size={12} />
                  {route.enabled ? t("ChannelsPanel.disable") : t("ChannelsPanel.enable")}
                </Button>
                <Button
                  size="sm"
                  variant="danger"
                  disabled={busy !== null}
                  onClick={() => void run(`remove-${route.route_id}`, () => channelsRemoveRoute(route.route_id))}
                >
                  <Trash2 size={12} />
                  {t("ChannelsPanel.remove")}
                </Button>
              </span>
            </li>
          ))}
        </ul>
      )}

      {draft && uses && (
        <div className="mt-4 border-t border-border pt-3">
          <div className="flex items-center justify-between">
            <h5 className="text-xs font-semibold">
              {draft.routeId ? t("ChannelsPanel.editRoute") : t("ChannelsPanel.addRoute")}
            </h5>
            <Button size="sm" onClick={() => setDraft(null)}>
              <X size={12} />
              {t("ChannelsPanel.cancel")}
            </Button>
          </div>

          <div className="mt-3 grid gap-2 sm:grid-cols-2">
            <div>
              <label className="text-xs text-muted">
                {t("ChannelsPanel.recipe")}
                <select
                  className={`${INPUT} mt-1`}
                  value={draft.recipe}
                  onChange={(event) => setDraft({ ...draft, recipe: event.target.value })}
                >
                  <option value="">{t("ChannelsPanel.recipePick")}</option>
                  {/* A route saved against a task that has since been renamed
                      or deleted keeps naming it here, rather than silently
                      reading as some other task the moment it is opened. */}
                  {draft.recipe.length > 0 && !recipeNames.includes(draft.recipe) && (
                    <option value={draft.recipe}>
                      {t("ChannelsPanel.recipeMissingOption", { name: draft.recipe })}
                    </option>
                  )}
                  {recipeNames.map((name) => (
                    <option key={name} value={name}>
                      {name}
                    </option>
                  ))}
                </select>
              </label>
              {draft.recipe.length > 0 && !recipeNames.includes(draft.recipe) && (
                <span className="mt-1 block text-xs text-warning">
                  {t("ChannelsPanel.recipeMissing")}
                </span>
              )}
              {recipeNames.length === 0 && (
                <Button
                  size="sm"
                  className="mt-2"
                  disabled={busy !== null}
                  onClick={() =>
                    void run("starter-recipe", async () =>
                      setDraft({ ...draft, recipe: await ensureStarterRecipe() }),
                    )
                  }
                >
                  <Plus size={12} />
                  {t("ChannelsPanel.starterRecipe")}
                </Button>
              )}
            </div>
            <label className="text-xs text-muted">
              {t("ChannelsPanel.scope")}
              <select
                className={`${INPUT} mt-1`}
                value={draft.level}
                onChange={(event) => setDraft({ ...draft, level: event.target.value as ScopeLevel })}
              >
                {(["sender", "thread", "conversation", "account", "provider", "global"] as ScopeLevel[]).map(
                  (level) => (
                    <option key={level} value={level}>
                      {t(`ChannelsPanel.level_${level}`)}
                    </option>
                  ),
                )}
              </select>
            </label>

            {uses.kind && (
              <label className="text-xs text-muted">
                {t("ChannelsPanel.provider")}
                <select
                  className={`${INPUT} mt-1`}
                  value={draft.kind}
                  onChange={(event) => setDraft({ ...draft, kind: event.target.value })}
                >
                  {[...new Set(accounts.map((account) => account.kind))].map((kind) => (
                    <option key={kind} value={kind}>
                      {kind}
                    </option>
                  ))}
                  {accounts.length === 0 && <option value={draft.kind}>{draft.kind}</option>}
                </select>
              </label>
            )}

            {uses.account && (
              <label className="text-xs text-muted">
                {t("ChannelsPanel.account")}
                <select
                  className={`${INPUT} mt-1`}
                  value={draft.accountId}
                  onChange={(event) => setDraft({ ...draft, accountId: event.target.value })}
                >
                  <option value="">{t("ChannelsPanel.chooseAccount")}</option>
                  {accounts.map((account) => (
                    <option key={account.account_id} value={account.account_id}>
                      {account.label || account.account_id}
                    </option>
                  ))}
                </select>
              </label>
            )}

            {uses.conversation && (
              <label className="text-xs text-muted">
                {t("ChannelsPanel.conversation")}
                <input
                  className={`${INPUT} mt-1`}
                  list="channel-conversations"
                  value={draft.conversationId}
                  onChange={(event) => setDraft({ ...draft, conversationId: event.target.value })}
                  placeholder={t("ChannelsPanel.conversationPlaceholder")}
                />
                <datalist id="channel-conversations">
                  {seen.conversations.map((id) => (
                    <option key={id} value={id} />
                  ))}
                </datalist>
              </label>
            )}

            {uses.thread && (
              <label className="text-xs text-muted">
                {t("ChannelsPanel.thread")}
                <input
                  className={`${INPUT} mt-1`}
                  list="channel-threads"
                  value={draft.threadId}
                  onChange={(event) => setDraft({ ...draft, threadId: event.target.value })}
                />
                <datalist id="channel-threads">
                  {seen.threads.map((id) => (
                    <option key={id} value={id} />
                  ))}
                </datalist>
              </label>
            )}

            {uses.sender && (
              <label className="text-xs text-muted">
                {t("ChannelsPanel.sender")}
                <input
                  className={`${INPUT} mt-1`}
                  list="channel-senders"
                  value={draft.senderId}
                  onChange={(event) => setDraft({ ...draft, senderId: event.target.value })}
                />
                <datalist id="channel-senders">
                  {seen.senders.map((id) => (
                    <option key={id} value={id} />
                  ))}
                </datalist>
              </label>
            )}

            <label className="text-xs text-muted">
              {t("ChannelsPanel.repository")}
              <input
                className={`${INPUT} mt-1`}
                value={draft.repository}
                onChange={(event) => setDraft({ ...draft, repository: event.target.value })}
                placeholder={t("ChannelsPanel.repositoryPlaceholder")}
              />
            </label>
            <label className="text-xs text-muted">
              {t("ChannelsPanel.sessionScope")}
              <select
                className={`${INPUT} mt-1`}
                value={draft.sessionScope}
                onChange={(event) => setDraft({ ...draft, sessionScope: event.target.value as SessionScope })}
              >
                {(["thread", "conversation", "sender", "account"] as SessionScope[]).map((scope) => (
                  <option key={scope} value={scope}>
                    {t(`ChannelsPanel.sessionScope_${scope}`)}
                  </option>
                ))}
              </select>
            </label>
            <label className="text-xs text-muted">
              {t("ChannelsPanel.priority")}
              <input
                className={`${INPUT} mt-1`}
                inputMode="numeric"
                value={draft.priority}
                onChange={(event) => setDraft({ ...draft, priority: event.target.value })}
                placeholder="0"
              />
            </label>
            <label className="flex items-center gap-2 text-xs text-muted sm:mt-5">
              <input
                type="checkbox"
                checked={draft.reply}
                onChange={(event) => setDraft({ ...draft, reply: event.target.checked })}
              />
              {t("ChannelsPanel.routeReply")}
            </label>
            <label className="flex items-center gap-2 text-xs text-muted">
              <input
                type="checkbox"
                checked={draft.enabled}
                onChange={(event) => setDraft({ ...draft, enabled: event.target.checked })}
              />
              {t("ChannelsPanel.routeEnabled")}
            </label>
          </div>

          {/* Recipe parameters, as the daemon stores them: name and value,
              nothing interpreted here. The rows loaded from an existing route
              are all sent back on save, because the daemon replaces the
              target wholesale. */}
          <div className="mt-3">
            <h6 className="text-xs font-semibold text-muted">{t("ChannelsPanel.routeParams")}</h6>
            {draft.params.map((row, index) => (
              // Position is the identity here: names are editable, so they
              // cannot key the row.
              <div key={index} className="mt-2 flex items-end gap-2">
                <label className="min-w-0 flex-1 text-xs text-muted">
                  {t("ChannelsPanel.paramName")}
                  <input
                    className={`${INPUT} mt-1`}
                    value={row.name}
                    onChange={(event) =>
                      setDraft({
                        ...draft,
                        params: draft.params.map((entry, at) =>
                          at === index ? { ...entry, name: event.target.value } : entry,
                        ),
                      })
                    }
                  />
                </label>
                <label className="min-w-0 flex-1 text-xs text-muted">
                  {t("ChannelsPanel.paramValue")}
                  <input
                    className={`${INPUT} mt-1`}
                    value={row.value}
                    onChange={(event) =>
                      setDraft({
                        ...draft,
                        params: draft.params.map((entry, at) =>
                          at === index ? { ...entry, value: event.target.value } : entry,
                        ),
                      })
                    }
                  />
                </label>
                <Button
                  size="sm"
                  aria-label={t("ChannelsPanel.removeParam")}
                  onClick={() =>
                    setDraft({ ...draft, params: draft.params.filter((_, at) => at !== index) })
                  }
                >
                  <X size={12} />
                </Button>
              </div>
            ))}
            {paramError && (
              <p className="mt-1 text-xs text-danger">{t(`ChannelsPanel.paramError_${paramError}`)}</p>
            )}
            <Button
              className="mt-2"
              size="sm"
              onClick={() => setDraft({ ...draft, params: [...draft.params, { name: "", value: "" }] })}
            >
              <Plus size={12} />
              {t("ChannelsPanel.addParam")}
            </Button>
          </div>

          <p className="mt-2 text-xs text-faint">{t("ChannelsPanel.routeReplyHint")}</p>

          <Button
            className="mt-2"
            size="sm"
            disabled={busy !== null || incomplete}
            onClick={() =>
              void run("save", async () => {
                const options = draftOptions(draft);
                if (draft.routeId) {
                  await channelsUpdateRoute(draft.routeId, draft.recipe.trim(), options);
                } else {
                  await channelsAddRoute(draft.recipe.trim(), options);
                }
                setDraft(null);
              })
            }
          >
            {busy === "save" ? <Loader2 size={14} className="animate-spin" /> : null}
            {t("ChannelsPanel.saveRoute")}
          </Button>
        </div>
      )}
    </section>
  );
}
