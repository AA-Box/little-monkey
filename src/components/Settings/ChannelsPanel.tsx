import { useCallback, useEffect, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { AlertTriangle, Check, Ban, Copy, ExternalLink, KeyRound, Loader2, Plug, Power, RefreshCw, Save, Trash2 } from "lucide-react";
import {
  type AccessPolicy,
  type ChannelAccount,
  type ChannelCallback,
  type ChannelEvent,
  type ChannelHealthState,
  type GroupActivation,
  type PendingSender,
  PROVIDER_GUIDES,
  UNIVERSAL_CONFIG_FIELDS,
  buildProviderConfig,
  channelsAdd,
  channelsCallbackUrl,
  channelsDecideSender,
  channelsEnable,
  channelsEvents,
  channelsList,
  channelsProbe,
  channelsRemove,
  channelsRoutes,
  channelsSenders,
  channelsSetConfig,
  channelsSetCredential,
  channelsSetPolicy,
  channelsSetPublicUrl,
  configFormValues,
  editableConfigFields,
  mergeProviderConfig,
  missingRequiredConfig,
  needsPublicCallback,
} from "../../lib/channelsClient";
import { Button } from "../ui";
import { ChannelRoutesSection } from "./ChannelRoutesSection";
import { IngressTurnsSection } from "./IngressTurnsSection";
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
  const field = editableConfigFields(kind).find((entry) => entry.key === key);
  return field?.label ?? key;
}

/** Settings the panel has no input for — keys an account picked up from the
 * terminal — are still shown, so an edit never hides what is configured. */
function formatConfigValue(value: unknown): string {
  if (Array.isArray(value)) return value.join(", ");
  if (typeof value === "string") return value;
  return JSON.stringify(value) ?? "";
}

/** One typed input for one non-secret setting, shared by the add flow and the
 * edit form so the two cannot drift in how they collect a value. */
function ConfigFieldInput({
  field,
  value,
  onChange,
  yesLabel,
  noLabel,
}: {
  field: import("../../lib/channelsClient").ProviderConfigField;
  value: string;
  onChange: (next: string) => void;
  yesLabel: string;
  noLabel: string;
}) {
  return (
    <label className="text-xs text-muted">
      {field.label}{field.required ? " *" : ""}
      {field.type === "boolean" ? (
        <select className={`${INPUT} mt-1`} value={value || "false"} onChange={(event) => onChange(event.target.value)}>
          <option value="false">{noLabel}</option>
          <option value="true">{yesLabel}</option>
        </select>
      ) : (
        <input
          className={`${INPUT} mt-1`}
          inputMode={field.type === "number" ? "numeric" : undefined}
          value={value}
          onChange={(event) => onChange(event.target.value)}
          placeholder={field.placeholder ?? ""}
        />
      )}
      {field.hint && <span className="mt-1 block text-faint">{field.hint}</span>}
    </label>
  );
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
  /** The selected account's canonical callback URL, as the daemon composes it.
   * Never assembled here: only the daemon knows what it is reachable as. */
  const [callback, setCallback] = useState<ChannelCallback | null>(null);
  const [publicUrlDraft, setPublicUrlDraft] = useState("");
  /** Open editor for the selected account's non-secret settings, or null while
   * they are only being displayed. */
  const [settingsDraft, setSettingsDraft] = useState<Record<string, string> | null>(null);
  const [labelDraft, setLabelDraft] = useState("");

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

  const loadDetail = useCallback(async (accountId: string, kind: string) => {
    try {
      const [waiting, recent] = await Promise.all([channelsSenders(accountId), channelsEvents(accountId, 20)]);
      setSenders(waiting.pending);
      setEvents(recent.events);
      // Only webhook providers have a callback to show, and asking the daemon
      // is the only way to learn whether one is reachable at all.
      setCallback(needsPublicCallback(kind) ? await channelsCallbackUrl(accountId) : null);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }, []);

  // Anything typed for one account is dropped when another is selected: a
  // half-entered credential must never be saved against a different account,
  // and neither must half-edited settings.
  const selectedKind = accounts?.find((entry) => entry.account_id === selected)?.kind ?? null;
  useEffect(() => {
    setSecret("");
    setSecretParts({});
    setSettingsDraft(null);
    setCallback(null);
    if (selected && selectedKind) void loadDetail(selected, selectedKind);
  }, [selected, selectedKind, loadDetail]);

  const run = useCallback(async (key: string, action: () => Promise<unknown>, done?: string) => {
    setBusy(key);
    setError(null);
    setNotice(null);
    try {
      await action();
      if (done) setNotice(done);
      await load();
      if (selected && selectedKind) await loadDetail(selected, selectedKind);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(null);
    }
  }, [load, loadDetail, selected, selectedKind]);

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
              {/* A provider whose deliveries stop authenticating has no other
                  symptom: the messages simply stop, health stays wherever the
                  last probe left it, and every field above still reads
                  correctly. This banner is the whole of what an operator has to
                  go on, which is why it names the count and the reason and
                  points at the two things that are actually wrong. */}
              {account.callback_rejections.count > 0 && (
                <p className="mt-2 flex items-start gap-2 rounded-md border border-danger/40 bg-danger/10 p-2 text-xs text-danger">
                  <AlertTriangle size={14} className="mt-0.5 shrink-0" />
                  <span>
                    {t("ChannelsPanel.callbacksRejected", { count: account.callback_rejections.count })}
                    {account.callback_rejections.last_reason && (
                      <span className="block text-faint">{account.callback_rejections.last_reason}</span>
                    )}
                  </span>
                </p>
              )}
              {settingsDraft === null && Object.keys(account.non_secret_config).length > 0 && (
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
              <Button
                size="sm"
                disabled={busy !== null}
                onClick={() => {
                  if (settingsDraft) {
                    setSettingsDraft(null);
                    return;
                  }
                  setLabelDraft(account.label);
                  setSettingsDraft(configFormValues(editableConfigFields(account.kind), account.non_secret_config));
                }}
              >
                <Save size={14} />{settingsDraft ? t("ChannelsPanel.cancel") : t("ChannelsPanel.editSettings")}
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

          {/* Editing what an account is configured against, not just reading
              it. The settings replace the stored object wholesale, so keys
              this panel has no input for are carried across untouched rather
              than dropped; the daemon validates the result against what the
              provider's adapter actually reads. */}
          {settingsDraft !== null && (
            <div className="mt-3 border-t border-border pt-3">
              <h5 className="text-xs font-semibold">{t("ChannelsPanel.settings")}</h5>
              <div className="mt-2 grid gap-2 sm:grid-cols-2">
                <label className="text-xs text-muted">{t("ChannelsPanel.label")}
                  <input className={`${INPUT} mt-1`} value={labelDraft} onChange={(event) => setLabelDraft(event.target.value)} />
                </label>
                {(guide?.configFields ?? []).map((field) => (
                  <ConfigFieldInput
                    key={field.key}
                    field={field}
                    value={settingsDraft[field.key] ?? ""}
                    onChange={(next) => setSettingsDraft({ ...settingsDraft, [field.key]: next })}
                    yesLabel={t("ChannelsPanel.yes")}
                    noLabel={t("ChannelsPanel.no")}
                  />
                ))}
              </div>
              {/* The knobs every account accepts regardless of provider: what
                  one message's files may cost. Separated so a provider's own
                  settings stay the short list an operator usually wants. */}
              <h6 className="mt-3 text-xs font-semibold text-muted">{t("ChannelsPanel.advancedSettings")}</h6>
              <div className="mt-2 grid gap-2 sm:grid-cols-2">
                {UNIVERSAL_CONFIG_FIELDS.map((field) => (
                  <ConfigFieldInput
                    key={field.key}
                    field={field}
                    value={settingsDraft[field.key] ?? ""}
                    onChange={(next) => setSettingsDraft({ ...settingsDraft, [field.key]: next })}
                    yesLabel={t("ChannelsPanel.yes")}
                    noLabel={t("ChannelsPanel.no")}
                  />
                ))}
              </div>
              <p className="mt-2 text-xs text-faint">{t("ChannelsPanel.settingsReprobeHint")}</p>
              <Button
                className="mt-2"
                size="sm"
                disabled={
                  busy !== null ||
                  labelDraft.trim().length === 0 ||
                  missingRequiredConfig(editableConfigFields(account.kind), settingsDraft).length > 0
                }
                onClick={() => void run("settings", async () => {
                  const merged = mergeProviderConfig(
                    account.non_secret_config,
                    editableConfigFields(account.kind),
                    settingsDraft,
                  );
                  await channelsSetConfig(account.account_id, JSON.stringify(merged), labelDraft.trim());
                  setSettingsDraft(null);
                }, t("ChannelsPanel.settingsSaved"))}
              ><Save size={14} />{t("ChannelsPanel.saveSettings")}</Button>
            </div>
          )}

          {needsPublicCallback(account.kind) && (
            <div className="mt-3 rounded-md border border-border bg-background p-2 text-xs text-muted">
              {callback?.configured && callback.url ? (
                <>
                  <p>{t("ChannelsPanel.callbackHint")}</p>
                  <p className="mt-1 flex flex-wrap items-center gap-2">
                    <code className="min-w-0 break-all text-foreground">{callback.url}</code>
                    <Button size="sm" onClick={() => void navigator.clipboard.writeText(callback.url ?? "")}>
                      <Copy size={12} />{t("ChannelsPanel.copy")}
                    </Button>
                  </p>
                </>
              ) : (
                <>
                  <p className="text-warning">{t("ChannelsPanel.callbackUnconfigured")}</p>
                  <p className="mt-1">
                    {t("ChannelsPanel.callbackPathIs")} <code className="text-foreground">{callback?.path ?? ""}</code>
                  </p>
                  <div className="mt-2 flex flex-col gap-2 sm:flex-row sm:items-end">
                    <label className="min-w-0 flex-1 text-xs text-muted">{t("ChannelsPanel.publicBaseUrl")}
                      <input
                        className={`${INPUT} mt-1`}
                        value={publicUrlDraft}
                        onChange={(event) => setPublicUrlDraft(event.target.value)}
                        placeholder="https://hooks.example.com"
                      />
                    </label>
                    <Button
                      size="sm"
                      disabled={busy !== null || publicUrlDraft.trim().length === 0}
                      onClick={() => void run("public-url", async () => {
                        await channelsSetPublicUrl(publicUrlDraft.trim());
                        setPublicUrlDraft("");
                      }, t("ChannelsPanel.publicBaseUrlSaved"))}
                    >{t("ChannelsPanel.savePublicBaseUrl")}</Button>
                  </div>
                  <p className="mt-1 text-faint">{t("ChannelsPanel.publicBaseUrlHint")}</p>
                </>
              )}
              {account.kind === "whatsapp" && <p className="mt-1">{t("ChannelsPanel.callbackVerifyHint")}</p>}
            </div>
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
              <Button size="sm" disabled={busy !== null} onClick={() => void loadDetail(account.account_id, account.kind)}><RefreshCw size={12} />{t("ChannelsPanel.refresh")}</Button>
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
              {PROVIDER_GUIDES.filter((entry) => !entry.editOnly).map((entry) => <option key={entry.kind} value={entry.kind}>{entry.label}</option>)}
            </select>
          </label>
          <label className="text-xs text-muted">{t("ChannelsPanel.label")}
            <input className={`${INPUT} mt-1`} value={draft.label} onChange={(event) => setDraft({ ...draft, label: event.target.value })} placeholder={t("ChannelsPanel.labelPlaceholder")} />
          </label>
          {(draftGuide?.configFields ?? []).map((field) => (
            <ConfigFieldInput
              key={field.key}
              field={field}
              value={configDraft[field.key] ?? ""}
              onChange={(next) => setConfigDraft({ ...configDraft, [field.key]: next })}
              yesLabel={t("ChannelsPanel.yes")}
              noLabel={t("ChannelsPanel.no")}
            />
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

      <ChannelRoutesSection accounts={accounts} onChanged={load} />

      <IngressTurnsSection />
    </div>
  );
}
