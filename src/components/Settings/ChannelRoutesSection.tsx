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

function draftFrom(route: ChannelRoute): RouteDraft {
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
    sessionScope: route.target.session_scope,
    priority: route.target.priority === 0 ? "" : String(route.target.priority),
    reply: route.target.reply_to_conversation,
    enabled: route.enabled,
  };
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
    session_scope: draft.sessionScope,
    priority: draft.priority.trim().length > 0 && Number.isFinite(priority) ? priority : null,
    reply: draft.reply,
    enabled: draft.enabled,
  };
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
  }, [load]);

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

  const uses = draft ? scopeFields(draft.level) : null;
  // Every id the chosen rung needs must be present before the daemon is asked:
  // a scope missing its account is refused there, and refusing here says so
  // while the operator is still looking at the field.
  const incomplete =
    draft !== null &&
    uses !== null &&
    (draft.recipe.trim().length === 0 ||
      (uses.account && draft.accountId.trim().length === 0) ||
      (uses.conversation && draft.conversationId.trim().length === 0) ||
      (uses.sender && draft.senderId.trim().length === 0));

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
        <p className="mt-3 text-xs text-muted">{t("ChannelsPanel.noRoutesYet")}</p>
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
            <label className="text-xs text-muted">
              {t("ChannelsPanel.recipe")}
              <input
                className={`${INPUT} mt-1`}
                value={draft.recipe}
                onChange={(event) => setDraft({ ...draft, recipe: event.target.value })}
                placeholder="chat"
              />
            </label>
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
                {draft.level === "sender" && <span className="text-faint"> ({t("ChannelsPanel.optional")})</span>}
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
