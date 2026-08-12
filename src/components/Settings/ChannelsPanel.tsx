import { useCallback, useEffect, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { AlertTriangle, Check, Ban, ExternalLink, KeyRound, Loader2, Plug, Power, RefreshCw, Trash2 } from "lucide-react";
import {
  type AccessPolicy,
  type ChannelAccount,
  type ChannelEvent,
  type ChannelHealthState,
  type GroupActivation,
  type PendingSender,
  PROVIDER_GUIDES,
  buildProviderConfig,
  callbackPath,
  channelsAdd,
  channelsAddRoute,
  channelsDecideSender,
  channelsEnable,
  channelsEvents,
  channelsList,
  channelsProbe,
  channelsRemove,
  channelsRoutes,
  channelsSenders,
  channelsSetCredential,
  channelsSetPolicy,
  missingRequiredConfig,
  needsPublicCallback,
} from "../../lib/channelsClient";
import { Button } from "../ui";
import { errorMessage } from "../../lib/errors";
import { useT } from "../../lib/i18n";

const INPUT = "w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent";

/**
 * Health is rendered from what the daemon last probed, never from whether a
 * credential exists — an account with a saved token and no successful probe
 * reads "not checked yet", which is the truth.
 */
function healthTone(state: ChannelHealthState): string {
  if (state === "connected") return "text-success";
  if (state === "degraded" || state === "connecting") return "text-warning";
  if (state === "error" || state === "unsupported") return "text-danger";
  return "text-muted";
}

/** The operator-facing name of a stored setting, falling back to the raw key
 * for anything a guide does not describe — an account configured from the
 * terminal can carry keys the UI has never heard of. */
function configLabel(kind: string, key: string): string {
  const field = PROVIDER_GUIDES.find((guide) => guide.kind === kind)?.configFields.find(
    (entry) => entry.key === key,
  );
  return field?.label ?? key;
}

/** Settings are shown, not edited, so a value only has to be legible.
 * Objects are rendered as JSON rather than `[object Object]`. */
function formatConfigValue(value: unknown): string {
  if (Array.isArray(value)) return value.join(", ");
  if (typeof value === "string") return value;
  return JSON.stringify(value) ?? "";
}

export function ChannelsPanel() {
  const { t } = useT();
  const [accounts, setAccounts] = useState<ChannelAccount[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [senders, setSenders] = useState<PendingSender[]>([]);
  const [events, setEvents] = useState<ChannelEvent[]>([]);
  const [routeCount, setRouteCount] = useState<number | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [draft, setDraft] = useState({ kind: "telegram", label: "" });
  const [configDraft, setConfigDraft] = useState<Record<string, string>>({});
  const [secret, setSecret] = useState("");
  const [secretParts, setSecretParts] = useState<Record<string, string>>({});
  const [routeDraft, setRouteDraft] = useState({ recipe: "", scope: "account" as "account" | "global" });

  const load = useCallback(async () => {
    try {
      const [listed, routes] = await Promise.all([channelsList(), channelsRoutes()]);
      setAccounts(listed.accounts);
      setRouteCount(routes.routes.length);
      setError(null);
    } catch (reason) {
      setError(errorMessage(reason));
      setAccounts([]);
    }
  }, []);

  useEffect(() => { void load(); }, [load]);

  const loadDetail = useCallback(async (accountId: string) => {
    try {
      const [waiting, recent] = await Promise.all([channelsSenders(accountId), channelsEvents(accountId, 20)]);
      setSenders(waiting.pending);
      setEvents(recent.events);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }, []);

  // Anything typed for one account is dropped when another is selected: a
  // half-entered credential must never be saved against a different account.
  useEffect(() => {
    setSecret("");
    setSecretParts({});
    if (selected) void loadDetail(selected);
  }, [selected, loadDetail]);

  const run = useCallback(async (key: string, action: () => Promise<unknown>, done?: string) => {
    setBusy(key);
    setError(null);
    setNotice(null);
    try {
      await action();
      if (done) setNotice(done);
      await load();
      if (selected) await loadDetail(selected);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(null);
    }
  }, [load, loadDetail, selected]);

  const account = accounts?.find((entry) => entry.account_id === selected) ?? null;
  const guide = PROVIDER_GUIDES.find((entry) => entry.kind === (account?.kind ?? draft.kind));
  const draftGuide = PROVIDER_GUIDES.find((entry) => entry.kind === draft.kind);
  const missingConfig = missingRequiredConfig(draftGuide?.configFields ?? [], configDraft);

  // A provider whose credential is several values is collected field by field
  // and saved as the one JSON bundle its adapter parses. Nobody should have to
  // know that shape, and typing it by hand is how a working token ends up
  // rejected as malformed.
  const fields = guide?.secretFields ?? [];
  const credentialValue = fields.length > 0
    ? JSON.stringify(Object.fromEntries(fields.map((field) => [field.key, secretParts[field.key] ?? ""])))
    : secret;
  const credentialReady = fields.length > 0
    ? fields.every((field) => (secretParts[field.key] ?? "").length > 0)
    : secret.length > 0;

  if (accounts === null) {
    return <p className="flex items-center gap-2 text-sm text-muted"><Loader2 size={14} className="animate-spin" />{t("ChannelsPanel.loading")}</p>;
  }

  return (
    <div className="flex flex-col gap-6">
      <section className="rounded-lg border border-border bg-surface p-4">
        <h3 className="text-sm font-semibold">{t("ChannelsPanel.title")}</h3>
        <p className="mt-1 text-xs leading-relaxed text-muted">{t("ChannelsPanel.intro")}</p>
        {routeCount === 0 && accounts.length > 0 && (
          <p className="mt-2 flex items-start gap-2 rounded-md border border-warning/40 bg-warning/10 p-2 text-xs text-warning">
            <AlertTriangle size={14} className="mt-0.5 shrink-0" />{t("ChannelsPanel.noRoutes")}
          </p>
        )}
      </section>

      {error && <p className="rounded-md border border-danger/40 bg-danger/10 p-2 text-xs text-danger">{error}</p>}
      {notice && <p className="rounded-md border border-border bg-background p-2 text-xs text-muted">{notice}</p>}

      <section className="rounded-lg border border-border bg-surface p-4">
        <h4 className="text-sm font-semibold">{t("ChannelsPanel.accounts")}</h4>
        {accounts.length === 0 ? (
          <p className="mt-2 text-xs text-muted">{t("ChannelsPanel.empty")}</p>
        ) : (
          <ul className="mt-3 flex flex-col gap-2">
            {accounts.map((entry) => (
              <li key={entry.account_id}>
                <button
                  type="button"
                  onClick={() => setSelected(entry.account_id === selected ? null : entry.account_id)}
                  className="flex w-full items-center justify-between gap-3 rounded-md border border-border bg-background px-3 py-2 text-left text-sm"
                >
                  <span className="min-w-0">
                    <span className="font-medium">{entry.label || entry.account_id}</span>
                    <span className="ml-2 text-xs text-faint">{entry.kind}</span>
                  </span>
                  <span className={`text-xs ${healthTone(entry.health)}`}>
                    {entry.enabled ? t(`ChannelsPanel.health_${entry.health}`) : t("ChannelsPanel.disabled")}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      {account && (
        <section className="rounded-lg border border-border bg-surface p-4">
          <div className="flex items-start justify-between gap-3">
            <div className="min-w-0">
              <h4 className="text-sm font-semibold">{account.label || account.account_id}</h4>
              <p className="mt-1 text-xs text-muted">
                {t("ChannelsPanel.transport")}: {guide ? t(`ChannelsPanel.transport_${guide.transport}`) : account.kind}
              </p>
              {/* What the last probe actually reported: the helper's version,
                  the homeserver identity it answered as, how many encrypted
                  events an adapter had to skip. Health with no detail is a
                  colour and nothing else. */}
              {account.health_detail && <p className="mt-1 text-xs text-muted">{account.health_detail}</p>}
              {account.last_error && <p className="mt-1 text-xs text-danger">{account.last_error}</p>}
              {account.credential_required && !account.has_credential && (
                <p className="mt-1 text-xs text-warning">{t("ChannelsPanel.needsCredential")}</p>
              )}
              {Object.keys(account.non_secret_config).length > 0 && (
                <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-3 text-xs text-faint">
                  {Object.entries(account.non_secret_config).map(([key, value]) => (
                    <div key={key} className="contents">
                      <dt>{configLabel(account.kind, key)}</dt>
                      <dd className="min-w-0 truncate text-muted">{formatConfigValue(value)}</dd>
                    </div>
                  ))}
                </dl>
              )}
            </div>
            <div className="flex flex-wrap gap-2">
              <Button size="sm" disabled={busy !== null} onClick={() => void run("probe", () => channelsProbe(account.account_id))}>
                {busy === "probe" ? <Loader2 size={14} className="animate-spin" /> : <Plug size={14} />}{t("ChannelsPanel.testConnection")}
              </Button>
              <Button size="sm" disabled={busy !== null} onClick={() => void run("enable", () => channelsEnable(account.account_id, !account.enabled))}>
                <Power size={14} />{account.enabled ? t("ChannelsPanel.disable") : t("ChannelsPanel.enable")}
              </Button>
              <Button size="sm" variant="danger" disabled={busy !== null} onClick={() => void run("remove", () => channelsRemove(account.account_id), t("ChannelsPanel.removed"))}>
                <Trash2 size={14} />{t("ChannelsPanel.remove")}
              </Button>
            </div>
          </div>

          <div className="mt-3 flex flex-col gap-2 border-t border-border pt-3 sm:flex-row sm:items-end">
            {!account.credential_required ? (
              // Signal and iMessage authenticate through the helper the
              // operator installed, and IRC without SASL has nothing to log in
              // with. Showing a credential box here would invite someone to
              // paste a secret nothing will ever read.
              <p className="min-w-0 flex-1 text-xs text-muted">{t("ChannelsPanel.noCredentialNeeded")}</p>
            ) : fields.length > 0 ? (
              <div className="grid min-w-0 flex-1 gap-2 sm:grid-cols-2">
                {fields.map((field) => (
                  <label key={field.key} className="min-w-0 text-xs text-muted">{field.label}
                    <input
                      className={`${INPUT} mt-1`}
                      type="password"
                      value={secretParts[field.key] ?? ""}
                      onChange={(event) => setSecretParts({ ...secretParts, [field.key]: event.target.value })}
                      placeholder="••••••••"
                    />
                  </label>
                ))}
              </div>
            ) : (
              <label className="min-w-0 flex-1 text-xs text-muted">{guide?.credentialLabel ?? t("ChannelsPanel.credential")}
                <input className={`${INPUT} mt-1`} type="password" value={secret} onChange={(event) => setSecret(event.target.value)} placeholder="••••••••" />
              </label>
            )}
            {account.credential_required && (
              <Button
                size="sm"
                disabled={busy !== null || !credentialReady}
                onClick={() => void run("secret", async () => {
                  await channelsSetCredential(account.account_id, credentialValue);
                  setSecret("");
                  setSecretParts({});
                }, t("ChannelsPanel.credentialSaved"))}
              ><KeyRound size={14} />{t("ChannelsPanel.saveCredential")}</Button>
            )}
          </div>
          {guide && (
            <p className="mt-2 text-xs text-faint">
              {guide.whereToGetIt}{" "}
              <button type="button" className="inline-flex items-center gap-1 underline" onClick={() => void openUrl(guide.docsUrl)}>
                {t("ChannelsPanel.setupInstructions")}<ExternalLink size={12} />
              </button>
            </p>
          )}

          {needsPublicCallback(account.kind) && (
            <p className="mt-3 rounded-md border border-border bg-background p-2 text-xs text-muted">
              {t("ChannelsPanel.callbackHint")} <code className="text-foreground">{callbackPath(account.account_id)}</code>
              {account.kind === "whatsapp" && <span className="mt-1 block">{t("ChannelsPanel.callbackVerifyHint")}</span>}
            </p>
          )}

          <div className="mt-3 grid gap-2 border-t border-border pt-3 sm:grid-cols-3">
            <label className="text-xs text-muted">{t("ChannelsPanel.dmPolicy")}
              <select
                className={`${INPUT} mt-1`}
                value={account.access_policy.direct}
                onChange={(event) => void run("policy", () => channelsSetPolicy(account.account_id, event.target.value as AccessPolicy, null, null))}
              >
                {(["pairing", "allow_list", "open", "disabled"] as AccessPolicy[]).map((value) => (
                  <option key={value} value={value}>{t(`ChannelsPanel.policy_${value}`)}</option>
                ))}
              </select>
            </label>
            <label className="text-xs text-muted">{t("ChannelsPanel.groupPolicy")}
              <select
                className={`${INPUT} mt-1`}
                value={account.access_policy.group}
                onChange={(event) => void run("policy", () => channelsSetPolicy(account.account_id, null, event.target.value as AccessPolicy, null))}
              >
                {(["allow_list", "pairing", "open", "disabled"] as AccessPolicy[]).map((value) => (
                  <option key={value} value={value}>{t(`ChannelsPanel.policy_${value}`)}</option>
                ))}
              </select>
            </label>
            <label className="text-xs text-muted">{t("ChannelsPanel.activation")}
              <select
                className={`${INPUT} mt-1`}
                value={account.access_policy.group_activation}
                onChange={(event) => void run("policy", () => channelsSetPolicy(account.account_id, null, null, event.target.value as GroupActivation))}
              >
                {(["mention_only", "always", "disabled"] as GroupActivation[]).map((value) => (
                  <option key={value} value={value}>{t(`ChannelsPanel.activation_${value}`)}</option>
                ))}
              </select>
            </label>
          </div>

          <div className="mt-3 border-t border-border pt-3">
            <h5 className="text-xs font-semibold">{t("ChannelsPanel.pendingSenders")}</h5>
            {senders.length === 0 ? (
              <p className="mt-1 text-xs text-faint">{t("ChannelsPanel.noPendingSenders")}</p>
            ) : (
              <ul className="mt-2 flex flex-col gap-1">
                {senders.map((sender) => (
                  <li key={sender.sender_id} className="flex items-center justify-between gap-2 rounded-md border border-border bg-background px-2 py-1 text-xs">
                    <span className="min-w-0 truncate">{sender.display_label ?? sender.sender_id}</span>
                    <span className="flex gap-1">
                      <Button size="sm" disabled={busy !== null} onClick={() => void run("approve", () => channelsDecideSender(account.account_id, sender.sender_id, true), t("ChannelsPanel.senderApproved"))}>
                        <Check size={12} />{t("ChannelsPanel.approve")}
                      </Button>
                      <Button size="sm" variant="danger" disabled={busy !== null} onClick={() => void run("block", () => channelsDecideSender(account.account_id, sender.sender_id, false))}>
                        <Ban size={12} />{t("ChannelsPanel.block")}
                      </Button>
                    </span>
                  </li>
                ))}
              </ul>
            )}
            <p className="mt-1 text-xs text-faint">{t("ChannelsPanel.approvalScope")}</p>
          </div>

          <div className="mt-3 border-t border-border pt-3">
            <div className="flex items-center justify-between">
              <h5 className="text-xs font-semibold">{t("ChannelsPanel.activity")}</h5>
              <Button size="sm" disabled={busy !== null} onClick={() => void loadDetail(account.account_id)}><RefreshCw size={12} />{t("ChannelsPanel.refresh")}</Button>
            </div>
            {events.length === 0 ? (
              <p className="mt-1 text-xs text-faint">{t("ChannelsPanel.noActivity")}</p>
            ) : (
              <ul className="mt-2 flex flex-col gap-1">
                {events.map((event) => (
                  <li key={event.event_id} className="flex items-center justify-between gap-2 text-xs text-muted">
                    <span className="truncate">{event.direction} · {event.conversation_id}</span>
                    <span className="text-faint">{event.ignore_reason ?? event.disposition}</span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </section>
      )}

      <section className="rounded-lg border border-border bg-surface p-4">
        <h4 className="text-sm font-semibold">{t("ChannelsPanel.addAccount")}</h4>
        <div className="mt-3 grid gap-2 sm:grid-cols-2">
          <label className="text-xs text-muted">{t("ChannelsPanel.provider")}
            <select
              className={`${INPUT} mt-1`}
              value={draft.kind}
              onChange={(event) => {
                // Settings typed for one provider mean nothing to another.
                setDraft({ ...draft, kind: event.target.value });
                setConfigDraft({});
              }}
            >
              {PROVIDER_GUIDES.map((entry) => <option key={entry.kind} value={entry.kind}>{entry.label}</option>)}
            </select>
          </label>
          <label className="text-xs text-muted">{t("ChannelsPanel.label")}
            <input className={`${INPUT} mt-1`} value={draft.label} onChange={(event) => setDraft({ ...draft, label: event.target.value })} placeholder={t("ChannelsPanel.labelPlaceholder")} />
          </label>
          {(draftGuide?.configFields ?? []).map((field) => (
            <label key={field.key} className="text-xs text-muted">
              {field.label}{field.required ? " *" : ""}
              {field.type === "boolean" ? (
                <select
                  className={`${INPUT} mt-1`}
                  value={configDraft[field.key] ?? "false"}
                  onChange={(event) => setConfigDraft({ ...configDraft, [field.key]: event.target.value })}
                >
                  <option value="false">{t("ChannelsPanel.no")}</option>
                  <option value="true">{t("ChannelsPanel.yes")}</option>
                </select>
              ) : (
                <input
                  className={`${INPUT} mt-1`}
                  inputMode={field.type === "number" ? "numeric" : undefined}
                  value={configDraft[field.key] ?? ""}
                  onChange={(event) => setConfigDraft({ ...configDraft, [field.key]: event.target.value })}
                  placeholder={field.placeholder ?? ""}
                />
              )}
              {field.hint && <span className="mt-1 block text-faint">{field.hint}</span>}
            </label>
          ))}
        </div>
        {draftGuide?.requiresPlatform === "macos" && (
          <p className="mt-2 text-xs text-warning">{t("ChannelsPanel.macOnly")}</p>
        )}
        {missingConfig.length > 0 && (
          <p className="mt-2 text-xs text-faint">{t("ChannelsPanel.missingSettings")} {missingConfig.join(", ")}</p>
        )}
        <p className="mt-2 text-xs text-faint">{t("ChannelsPanel.addHint")}</p>
        <Button
          className="mt-2"
          size="sm"
          disabled={busy !== null || draft.label.trim().length === 0 || missingConfig.length > 0}
          onClick={() => void run("add", async () => {
            const config = buildProviderConfig(draftGuide?.configFields ?? [], configDraft);
            await channelsAdd(
              draft.kind,
              draft.label.trim(),
              Object.keys(config).length > 0 ? JSON.stringify(config) : null,
            );
            setConfigDraft({});
          }, t("ChannelsPanel.added"))}
        >{t("ChannelsPanel.add")}</Button>
      </section>

      <section className="rounded-lg border border-border bg-surface p-4">
        <h4 className="text-sm font-semibold">{t("ChannelsPanel.routes")}</h4>
        <p className="mt-1 text-xs text-muted">{t("ChannelsPanel.routesIntro")}</p>
        <div className="mt-3 flex flex-col gap-2 sm:flex-row sm:items-end">
          <label className="min-w-0 flex-1 text-xs text-muted">{t("ChannelsPanel.recipe")}
            <input className={`${INPUT} mt-1`} value={routeDraft.recipe} onChange={(event) => setRouteDraft({ ...routeDraft, recipe: event.target.value })} placeholder="chat" />
          </label>
          <label className="text-xs text-muted">{t("ChannelsPanel.scope")}
            <select className={`${INPUT} mt-1`} value={routeDraft.scope} onChange={(event) => setRouteDraft({ ...routeDraft, scope: event.target.value as "account" | "global" })}>
              <option value="account">{t("ChannelsPanel.scopeAccount")}</option>
              <option value="global">{t("ChannelsPanel.scopeGlobal")}</option>
            </select>
          </label>
          <Button
            size="sm"
            disabled={busy !== null || routeDraft.recipe.trim().length === 0 || (routeDraft.scope === "account" && !selected)}
            onClick={() => void run("route", () => channelsAddRoute(routeDraft.recipe.trim(), routeDraft.scope === "account" ? selected : null, null, null, null), t("ChannelsPanel.routeAdded"))}
          >{t("ChannelsPanel.addRoute")}</Button>
        </div>
      </section>
    </div>
  );
}
