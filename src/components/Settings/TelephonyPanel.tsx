import { useCallback, useEffect, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  AlertTriangle,
  Check,
  Copy,
  ExternalLink,
  KeyRound,
  Link2,
  Loader2,
  MessageSquare,
  Phone,
  Plug,
  Power,
  Trash2,
} from "lucide-react";
import {
  CARRIER_GUIDES,
  type InboundCallPolicy,
  type OutboundCallApproval,
  type TelecomAccount,
  type TelecomCall,
  type TelecomMessage,
  callbackPath,
  callbackUrl,
  statusCallbackUrl,
  telecomAdd,
  telecomCalls,
  telecomEnable,
  telecomList,
  telecomMessages,
  telecomProbe,
  telecomRemove,
  telecomSetCredential,
  telecomSetGreeting,
  telecomSetLimits,
  telecomSetPolicy,
  telecomSetPublicUrl,
} from "../../lib/telecomClient";
import type { ChannelHealthState } from "../../lib/channelsClient";
import { Button } from "../ui";
import { errorMessage } from "../../lib/errors";
import { useT } from "../../lib/i18n";

const INPUT =
  "w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent";

/** Health comes from the daemon's last probe, never from having saved a
 * credential: a token nobody has tried reads "not checked yet". */
function healthTone(state: ChannelHealthState): string {
  if (state === "connected") return "text-success";
  if (state === "degraded" || state === "connecting") return "text-warning";
  if (state === "error" || state === "unsupported") return "text-danger";
  return "text-muted";
}

export function TelephonyPanel() {
  const { t } = useT();
  const [accounts, setAccounts] = useState<TelecomAccount[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [calls, setCalls] = useState<TelecomCall[]>([]);
  const [messages, setMessages] = useState<TelecomMessage[]>([]);
  const [copied, setCopied] = useState(false);
  const [publicUrl, setPublicUrl] = useState("");
  const [config, setConfig] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [secret, setSecret] = useState("");
  const [greeting, setGreeting] = useState("");
  const [draft, setDraft] = useState({
    kind: "twilio",
    label: "",
    carrierAccountId: "",
    fromNumber: "",
    publicUrl: "",
    config: "",
  });

  const load = useCallback(async () => {
    try {
      setAccounts(await telecomList());
      setError(null);
    } catch (reason) {
      setError(errorMessage(reason));
      setAccounts([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const loadCalls = useCallback(async (accountId: string) => {
    try {
      const [recentCalls, recentMessages] = await Promise.all([
        telecomCalls(accountId, 20),
        telecomMessages(accountId, 20),
      ]);
      setCalls(recentCalls);
      setMessages(recentMessages);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }, []);

  useEffect(() => {
    if (selected) void loadCalls(selected);
  }, [selected, loadCalls]);

  useEffect(() => {
    const account = accounts?.find((entry) => entry.account_id === selected);
    setGreeting(account?.greeting ?? "");
    setPublicUrl(account?.public_base_url ?? "");
    setConfig("");
    setCopied(false);
  }, [selected, accounts]);

  const run = useCallback(
    async (key: string, action: () => Promise<unknown>, done?: string) => {
      setBusy(key);
      setError(null);
      setNotice(null);
      try {
        await action();
        if (done) setNotice(done);
        await load();
        if (selected) await loadCalls(selected);
      } catch (reason) {
        setError(errorMessage(reason));
      } finally {
        setBusy(null);
      }
    },
    [load, loadCalls, selected],
  );

  const account = accounts?.find((entry) => entry.account_id === selected) ?? null;
  const guide = CARRIER_GUIDES.find((entry) => entry.kind === (account?.kind ?? draft.kind));

  if (accounts === null) {
    return (
      <p className="flex items-center gap-2 text-sm text-muted">
        <Loader2 size={14} className="animate-spin" />
        {t("TelephonyPanel.loading")}
      </p>
    );
  }

  return (
    <div className="flex flex-col gap-6">
      <section className="rounded-lg border border-border bg-surface p-4">
        <h3 className="text-sm font-semibold">{t("TelephonyPanel.title")}</h3>
        <p className="mt-1 text-xs leading-relaxed text-muted">{t("TelephonyPanel.intro")}</p>
        <p className="mt-2 flex items-start gap-2 rounded-md border border-warning/40 bg-warning/10 p-2 text-xs text-warning">
          <AlertTriangle size={14} className="mt-0.5 shrink-0" />
          {t("TelephonyPanel.chargeWarning")}
        </p>
      </section>

      {error && (
        <p className="rounded-md border border-danger/40 bg-danger/10 p-2 text-xs text-danger">{error}</p>
      )}
      {notice && (
        <p className="rounded-md border border-border bg-background p-2 text-xs text-muted">{notice}</p>
      )}

      <section className="rounded-lg border border-border bg-surface p-4">
        <h4 className="text-sm font-semibold">{t("TelephonyPanel.numbers")}</h4>
        {accounts.length === 0 ? (
          <p className="mt-2 text-xs text-muted">{t("TelephonyPanel.empty")}</p>
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
                    <span className="font-medium">{entry.from_number}</span>
                    <span className="ml-2 text-xs text-faint">
                      {entry.kind_label} · {entry.label || entry.account_id}
                    </span>
                  </span>
                  <span className={`text-xs ${healthTone(entry.health.state)}`}>
                    {entry.enabled
                      ? t(`TelephonyPanel.health_${entry.health.state}`)
                      : t("TelephonyPanel.disabled")}
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
              <h4 className="text-sm font-semibold">{account.from_number}</h4>
              <p className="mt-1 text-xs text-muted">{account.kind_label}</p>
              {account.health.last_error && (
                <p className="mt-1 text-xs text-danger">{account.health.last_error}</p>
              )}
              {!account.has_credential && (
                <p className="mt-1 text-xs text-warning">{t("TelephonyPanel.needsCredential")}</p>
              )}
              {account.public_base_url === null && (
                <p className="mt-1 text-xs text-warning">{t("TelephonyPanel.needsPublicUrl")}</p>
              )}
            </div>
            <div className="flex flex-wrap gap-2">
              <Button
                size="sm"
                disabled={busy !== null}
                onClick={() => void run("probe", () => telecomProbe(account.account_id))}
              >
                {busy === "probe" ? <Loader2 size={14} className="animate-spin" /> : <Plug size={14} />}
                {t("TelephonyPanel.testConnection")}
              </Button>
              <Button
                size="sm"
                disabled={busy !== null}
                onClick={() => void run("enable", () => telecomEnable(account.account_id, !account.enabled))}
              >
                <Power size={14} />
                {account.enabled ? t("TelephonyPanel.disable") : t("TelephonyPanel.enable")}
              </Button>
              <Button
                size="sm"
                variant="danger"
                disabled={busy !== null}
                onClick={() =>
                  void run("remove", () => telecomRemove(account.account_id), t("TelephonyPanel.removed"))
                }
              >
                <Trash2 size={14} />
                {t("TelephonyPanel.remove")}
              </Button>
            </div>
          </div>

          <div className="mt-3 flex flex-col gap-2 border-t border-border pt-3 sm:flex-row sm:items-end">
            <label className="min-w-0 flex-1 text-xs text-muted">
              {guide?.credentialLabel ?? t("TelephonyPanel.credential")}
              <input
                className={`${INPUT} mt-1`}
                type="password"
                value={secret}
                onChange={(event) => setSecret(event.target.value)}
                placeholder="••••••••"
              />
            </label>
            <Button
              size="sm"
              disabled={busy !== null || secret.length === 0}
              onClick={() =>
                void run(
                  "secret",
                  async () => {
                    await telecomSetCredential(account.account_id, secret);
                    setSecret("");
                  },
                  t("TelephonyPanel.credentialSaved"),
                )
              }
            >
              <KeyRound size={14} />
              {t("TelephonyPanel.saveCredential")}
            </Button>
          </div>
          {guide && (
            <p className="mt-2 text-xs text-faint">
              {guide.whereToGetIt}{" "}
              <button
                type="button"
                className="inline-flex items-center gap-1 underline"
                onClick={() => void openUrl(guide.docsUrl)}
              >
                {t("TelephonyPanel.setupInstructions")}
                <ExternalLink size={12} />
              </button>
            </p>
          )}

          <div className="mt-3 border-t border-border pt-3">
            <p className="text-xs text-muted">{t("TelephonyPanel.callbackHint")}</p>
            <div className="mt-1 flex flex-wrap items-center gap-2">
              <code className="min-w-0 flex-1 truncate rounded bg-background px-1 py-1 text-xs">
                {callbackUrl(account) ?? callbackPath(account.account_id)}
              </code>
              <Button
                size="sm"
                disabled={callbackUrl(account) === null}
                onClick={() => {
                  const url = callbackUrl(account);
                  if (url === null) return;
                  void navigator.clipboard.writeText(url).then(() => setCopied(true));
                }}
              >
                {copied ? <Check size={14} /> : <Copy size={14} />}
                {copied ? t("TelephonyPanel.copied") : t("TelephonyPanel.copyCallbackUrl")}
              </Button>
            </div>
            <p className="mt-2 text-xs text-muted">{t("TelephonyPanel.statusHint")}</p>
            <code className="mt-1 block truncate rounded bg-background px-1 py-1 text-xs">
              {statusCallbackUrl(account) ?? `${callbackPath(account.account_id)}/status`}
            </code>
            {/* The URL a carrier posts to is what its signature covers, so a
                tunnel that moved rejects every genuine callback until this is
                fixed — which is why it is editable here and not only at setup. */}
            <div className="mt-2 flex flex-col gap-2 sm:flex-row sm:items-end">
              <label className="min-w-0 flex-1 text-xs text-muted">
                {t("TelephonyPanel.publicUrlEdit")}
                <input
                  className={`${INPUT} mt-1`}
                  value={publicUrl}
                  onChange={(event) => setPublicUrl(event.target.value)}
                  placeholder="https://calls.example.com"
                />
              </label>
              <Button
                size="sm"
                disabled={busy !== null || publicUrl.trim() === (account.public_base_url ?? "")}
                onClick={() =>
                  void run(
                    "publicUrl",
                    () => telecomSetPublicUrl(account.account_id, publicUrl.trim() || null),
                    t("TelephonyPanel.publicUrlSaved"),
                  )
                }
              >
                <Link2 size={14} />
                {t("TelephonyPanel.savePublicUrl")}
              </Button>
            </div>
            {/* A carrier rotates its published key and every callback stops
                verifying. Without this the only fix would be deleting the
                number and losing its history. */}
            {guide && guide.configKeys.length > 0 && (
              <div className="mt-2 flex flex-col gap-2 sm:flex-row sm:items-end">
                <label className="min-w-0 flex-1 text-xs text-muted">
                  {t("TelephonyPanel.settingsJsonEdit")} ({guide.configKeys.join(", ")})
                  <input
                    className={`${INPUT} mt-1`}
                    value={config}
                    onChange={(event) => setConfig(event.target.value)}
                    placeholder={`{"${guide.configKeys[0]}": "…"}`}
                  />
                </label>
                <Button
                  size="sm"
                  disabled={busy !== null || config.trim().length === 0}
                  onClick={() =>
                    void run(
                      "config",
                      async () => {
                        await telecomSetPublicUrl(account.account_id, publicUrl.trim() || null, config.trim());
                        setConfig("");
                      },
                      t("TelephonyPanel.settingsSaved"),
                    )
                  }
                >
                  {t("TelephonyPanel.saveSettings")}
                </Button>
              </div>
            )}
            {account.callback_rejections.count > 0 && (
              <p className="mt-2 flex items-start gap-2 rounded-md border border-danger/40 bg-danger/10 p-2 text-xs text-danger">
                <AlertTriangle size={14} className="mt-0.5 shrink-0" />
                <span>
                  {t("TelephonyPanel.callbacksRejected", {
                    count: account.callback_rejections.count,
                  })}
                  {account.callback_rejections.last_reason && (
                    <span className="block text-faint">
                      {account.callback_rejections.last_reason}
                    </span>
                  )}
                </span>
              </p>
            )}
          </div>

          <div className="mt-3 grid gap-3 border-t border-border pt-3 sm:grid-cols-2">
            <label className="text-xs text-muted">
              {t("TelephonyPanel.inboundPolicy")}
              <select
                className={`${INPUT} mt-1`}
                value={account.inbound_policy}
                disabled={busy !== null}
                onChange={(event) =>
                  void run("inbound", () =>
                    telecomSetPolicy(
                      account.account_id,
                      event.target.value as InboundCallPolicy,
                      null,
                    ),
                  )
                }
              >
                <option value="reject">{t("TelephonyPanel.inbound_reject")}</option>
                <option value="voicemail">{t("TelephonyPanel.inbound_voicemail")}</option>
                <option value="answer">{t("TelephonyPanel.inbound_answer")}</option>
              </select>
            </label>
            <label className="text-xs text-muted">
              {t("TelephonyPanel.outboundApproval")}
              <select
                className={`${INPUT} mt-1`}
                value={account.outbound_approval}
                disabled={busy !== null}
                onChange={(event) =>
                  void run("outbound", () =>
                    telecomSetPolicy(
                      account.account_id,
                      null,
                      event.target.value as OutboundCallApproval,
                    ),
                  )
                }
              >
                <option value="never">{t("TelephonyPanel.outbound_never")}</option>
                <option value="approval">{t("TelephonyPanel.outbound_approval")}</option>
                <option value="allow">{t("TelephonyPanel.outbound_allow")}</option>
              </select>
            </label>
          </div>
          <p className="mt-2 text-xs text-faint">{t("TelephonyPanel.policyScope")}</p>

          <div className="mt-3 flex flex-col gap-2 border-t border-border pt-3 sm:flex-row sm:items-end">
            <label className="min-w-0 flex-1 text-xs text-muted">
              {t("TelephonyPanel.greeting")}
              <input
                className={`${INPUT} mt-1`}
                value={greeting}
                onChange={(event) => setGreeting(event.target.value)}
                placeholder={t("TelephonyPanel.greetingPlaceholder")}
              />
            </label>
            <Button
              size="sm"
              disabled={busy !== null}
              onClick={() =>
                void run(
                  "greeting",
                  () => telecomSetGreeting(account.account_id, greeting.trim()),
                  t("TelephonyPanel.greetingSaved"),
                )
              }
            >
              {t("TelephonyPanel.saveGreeting")}
            </Button>
          </div>
          {account.inbound_policy !== "reject" && !account.greeting && (
            <p className="mt-1 text-xs text-warning">{t("TelephonyPanel.noGreeting")}</p>
          )}

          <div className="mt-3 grid gap-3 border-t border-border pt-3 sm:grid-cols-3">
            <label className="text-xs text-muted">
              {t("TelephonyPanel.maxConcurrent")}
              <input
                className={`${INPUT} mt-1`}
                type="number"
                min={1}
                defaultValue={account.limits.max_concurrent_calls}
                disabled={busy !== null}
                onBlur={(event) =>
                  void run("limits", () =>
                    telecomSetLimits(account.account_id, {
                      maxConcurrent: Number(event.target.value),
                    }),
                  )
                }
              />
            </label>
            <label className="text-xs text-muted">
              {t("TelephonyPanel.ringTimeout")}
              <input
                className={`${INPUT} mt-1`}
                type="number"
                min={5}
                defaultValue={account.limits.ring_timeout_s}
                disabled={busy !== null}
                onBlur={(event) =>
                  void run("limits", () =>
                    telecomSetLimits(account.account_id, {
                      ringTimeoutS: Number(event.target.value),
                    }),
                  )
                }
              />
            </label>
            <label className="text-xs text-muted">
              {t("TelephonyPanel.maxDuration")}
              <input
                className={`${INPUT} mt-1`}
                type="number"
                min={30}
                defaultValue={account.limits.max_duration_s}
                disabled={busy !== null}
                onBlur={(event) =>
                  void run("limits", () =>
                    telecomSetLimits(account.account_id, {
                      maxDurationS: Number(event.target.value),
                    }),
                  )
                }
              />
            </label>
          </div>
          <label className="mt-2 flex items-center gap-2 text-xs text-muted">
            <input
              type="checkbox"
              checked={account.limits.recording_enabled}
              disabled={busy !== null || !account.supports_recording}
              onChange={(event) =>
                void run("limits", () =>
                  telecomSetLimits(account.account_id, { recording: event.target.checked }),
                )
              }
            />
            {t("TelephonyPanel.recording")}
          </label>
          <p className="mt-1 text-xs text-faint">
            {account.supports_recording
              ? t("TelephonyPanel.recordingHint")
              : t("TelephonyPanel.recordingUnsupported")}
          </p>

          <div className="mt-3 border-t border-border pt-3">
            <h5 className="text-xs font-semibold">{t("TelephonyPanel.recentMessages")}</h5>
            {messages.length === 0 ? (
              <p className="mt-1 text-xs text-muted">{t("TelephonyPanel.noMessages")}</p>
            ) : (
              <ul className="mt-2 flex flex-col gap-1">
                {messages.map((message) => (
                  <li
                    key={`${message.direction}-${message.at_ms}-${message.peer_number}-${message.text}`}
                    className="flex items-center justify-between gap-2 text-xs"
                  >
                    <span className="min-w-0 truncate">
                      <MessageSquare size={11} className="mr-1 inline" />
                      {message.direction === "inbound"
                        ? t("TelephonyPanel.inbound")
                        : t("TelephonyPanel.outbound")}{" "}
                      {message.peer_number} — {message.text}
                    </span>
                    {/* A carrier's "never arrived" is a different answer from
                        "we sent it", and the one an operator is looking for. */}
                    <span
                      className={
                        message.delivery_state === "undelivered" || message.error
                          ? "shrink-0 text-danger"
                          : "shrink-0 text-faint"
                      }
                      title={message.error ?? undefined}
                    >
                      {message.delivery_state ?? message.state}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </div>

          <div className="mt-3 border-t border-border pt-3">
            <h5 className="text-xs font-semibold">{t("TelephonyPanel.recentCalls")}</h5>
            {calls.length === 0 ? (
              <p className="mt-1 text-xs text-muted">{t("TelephonyPanel.noCalls")}</p>
            ) : (
              <ul className="mt-2 flex flex-col gap-1">
                {calls.map((call) => (
                  <li key={call.call_id} className="flex items-center justify-between gap-2 text-xs">
                    <span className="min-w-0 truncate">
                      <Phone size={11} className="mr-1 inline" />
                      {call.direction === "inbound"
                        ? t("TelephonyPanel.inbound")
                        : t("TelephonyPanel.outbound")}{" "}
                      {call.peer_number}
                    </span>
                    {/* Why a call ended is the whole story of a call that
                        ended badly — a dropped media stream reads as an
                        ordinary "completed" without it. */}
                    <span className="text-faint" title={call.last_error ?? undefined}>
                      {call.state}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </section>
      )}

      <section className="rounded-lg border border-border bg-surface p-4">
        <h4 className="text-sm font-semibold">{t("TelephonyPanel.addNumber")}</h4>
        <div className="mt-3 grid gap-3 sm:grid-cols-2">
          <label className="text-xs text-muted">
            {t("TelephonyPanel.carrier")}
            <select
              className={`${INPUT} mt-1`}
              value={draft.kind}
              onChange={(event) => setDraft({ ...draft, kind: event.target.value })}
            >
              {CARRIER_GUIDES.map((entry) => (
                <option key={entry.kind} value={entry.kind}>
                  {entry.label}
                </option>
              ))}
            </select>
          </label>
          <label className="text-xs text-muted">
            {t("TelephonyPanel.label")}
            <input
              className={`${INPUT} mt-1`}
              value={draft.label}
              onChange={(event) => setDraft({ ...draft, label: event.target.value })}
              placeholder={t("TelephonyPanel.labelPlaceholder")}
            />
          </label>
          <label className="text-xs text-muted">
            {guide?.accountIdLabel ?? t("TelephonyPanel.carrierAccountId")}
            <input
              className={`${INPUT} mt-1`}
              value={draft.carrierAccountId}
              onChange={(event) => setDraft({ ...draft, carrierAccountId: event.target.value })}
            />
          </label>
          <label className="text-xs text-muted">
            {t("TelephonyPanel.fromNumber")}
            <input
              className={`${INPUT} mt-1`}
              value={draft.fromNumber}
              onChange={(event) => setDraft({ ...draft, fromNumber: event.target.value })}
              placeholder="+15551234567"
            />
          </label>
          <label className="text-xs text-muted sm:col-span-2">
            {t("TelephonyPanel.publicUrl")}
            <input
              className={`${INPUT} mt-1`}
              value={draft.publicUrl}
              onChange={(event) => setDraft({ ...draft, publicUrl: event.target.value })}
              placeholder="https://calls.example.com"
            />
          </label>
          {guide && guide.configKeys.length > 0 && (
            <label className="text-xs text-muted sm:col-span-2">
              {t("TelephonyPanel.settingsJson")} ({guide.configKeys.join(", ")})
              <input
                className={`${INPUT} mt-1`}
                value={draft.config}
                onChange={(event) => setDraft({ ...draft, config: event.target.value })}
                placeholder={`{"${guide.configKeys[0]}": "…"}`}
              />
            </label>
          )}
        </div>
        <p className="mt-2 text-xs text-faint">{t("TelephonyPanel.addHint")}</p>
        <Button
          className="mt-3"
          size="sm"
          disabled={
            busy !== null ||
            draft.label.trim().length === 0 ||
            draft.carrierAccountId.trim().length === 0 ||
            !draft.fromNumber.trim().startsWith("+")
          }
          onClick={() =>
            void run(
              "add",
              () =>
                telecomAdd(
                  draft.kind,
                  draft.label.trim(),
                  draft.carrierAccountId.trim(),
                  draft.fromNumber.trim(),
                  draft.publicUrl.trim() || null,
                  draft.config.trim() || null,
                ),
              t("TelephonyPanel.added"),
            )
          }
        >
          {t("TelephonyPanel.add")}
        </Button>
      </section>
    </div>
  );
}
