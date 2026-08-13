import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, Check, Fingerprint, Loader2, Send, Trash2, UserPlus } from "lucide-react";
import {
  type InboundPeer,
  type OutboundPeer,
  type PeerGrant,
  type PeerThread,
  PEER_GRANTS,
  formatFingerprint,
  hasRejection,
  peersAccept,
  peersGrant,
  peersInvite,
  peersList,
  peersRevoke,
  peersThreads,
  standingSummary,
} from "../../lib/peersClient";
import { Button } from "../ui";
import { errorMessage } from "../../lib/errors";
import { useT } from "../../lib/i18n";

const INPUT =
  "w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent";

type Translate = (key: string, params?: Record<string, string | number>) => string;

/** What a pairing means, said in words rather than implied by a colour.
 *
 * Spelled out one branch per state instead of building a key, so every string
 * this panel can render is greppable and the i18n lint can see it. */
function standingLabel(standing: ReturnType<typeof standingSummary>, t: Translate): string {
  if (standing === "revoked") return t("PeersPanel.standingRevoked");
  if (standing === "no-grants") return t("PeersPanel.standingNoGrants");
  if (standing === "mixed") return t("PeersPanel.standingMixed");
  return t("PeersPanel.standingPeerOnly");
}

function grantLabel(grant: PeerGrant, t: Translate): string {
  if (grant === "message") return t("PeersPanel.grantMessage");
  if (grant === "task") return t("PeersPanel.grantTask");
  return t("PeersPanel.grantArtifact");
}

export function PeersPanel() {
  const { t } = useT();
  const [inbound, setInbound] = useState<InboundPeer[] | null>(null);
  const [outbound, setOutbound] = useState<OutboundPeer[]>([]);
  const [threads, setThreads] = useState<PeerThread[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [inviteDraft, setInviteDraft] = useState<{ label: string; grants: PeerGrant[]; output: string }>({
    label: "",
    grants: ["message"],
    output: "",
  });
  const [acceptDraft, setAcceptDraft] = useState({ invitation: "", alias: "" });
  const [confirmRevoke, setConfirmRevoke] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [peers, opened] = await Promise.all([peersList(), peersThreads(null, 20)]);
      setInbound(peers.inbound);
      setOutbound(peers.outbound);
      setThreads(opened.threads);
      setError(null);
    } catch (reason) {
      setError(errorMessage(reason));
      setInbound([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const run = useCallback(
    async (key: string, action: () => Promise<string | null>) => {
      setBusy(key);
      setError(null);
      setNotice(null);
      try {
        const message = await action();
        if (message) setNotice(message);
        await load();
      } catch (reason) {
        setError(errorMessage(reason));
      } finally {
        setBusy(null);
      }
    },
    [load],
  );

  const toggleGrant = (grants: PeerGrant[], grant: PeerGrant): PeerGrant[] =>
    grants.includes(grant) ? grants.filter((value) => value !== grant) : [...grants, grant];

  if (inbound === null) {
    return (
      <div className="flex items-center gap-2 text-sm text-muted">
        <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
        {t("PeersPanel.loading")}
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <header className="space-y-1">
        <h2 className="text-base font-semibold text-foreground">{t("PeersPanel.title")}</h2>
        <p className="text-sm text-muted">{t("PeersPanel.intro")}</p>
      </header>

      {error && (
        <p role="alert" className="flex items-start gap-2 rounded-md border border-danger/40 bg-danger/10 px-3 py-2 text-sm text-danger">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
          {error}
        </p>
      )}
      {notice && (
        <p className="flex items-start gap-2 rounded-md border border-success/40 bg-success/10 px-3 py-2 text-sm text-success">
          <Check className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
          {notice}
        </p>
      )}

      <section className="space-y-3 rounded-lg border border-border bg-surface p-4">
        <h3 className="text-sm font-semibold text-foreground">{t("PeersPanel.inviteTitle")}</h3>
        <p className="text-sm text-muted">{t("PeersPanel.inviteDetail")}</p>
        <input
          className={INPUT}
          placeholder={t("PeersPanel.inviteLabelPlaceholder")}
          value={inviteDraft.label}
          onChange={(event) => setInviteDraft({ ...inviteDraft, label: event.target.value })}
        />
        <input
          className={INPUT}
          placeholder={t("PeersPanel.inviteOutputPlaceholder")}
          value={inviteDraft.output}
          onChange={(event) => setInviteDraft({ ...inviteDraft, output: event.target.value })}
        />
        <fieldset className="space-y-2">
          <legend className="text-xs font-medium uppercase tracking-wide text-muted">{t("PeersPanel.grantsLegend")}</legend>
          {PEER_GRANTS.map((grant) => (
            <label key={grant.id} className="flex items-start gap-2 text-sm text-foreground">
              <input
                type="checkbox"
                className="mt-1"
                checked={inviteDraft.grants.includes(grant.id)}
                onChange={() => setInviteDraft({ ...inviteDraft, grants: toggleGrant(inviteDraft.grants, grant.id) })}
              />
              <span>
                {t(grant.labelKey)}
                <span className="block text-xs text-muted">{t(grant.detailKey)}</span>
              </span>
            </label>
          ))}
        </fieldset>
        <Button
          disabled={busy !== null || inviteDraft.label.trim() === "" || inviteDraft.output.trim() === "" || inviteDraft.grants.length === 0}
          onClick={() =>
            run("invite", async () => {
              const created = await peersInvite(inviteDraft.label.trim(), inviteDraft.grants, 60, inviteDraft.output.trim());
              setInviteDraft({ label: "", grants: ["message"], output: "" });
              return t("PeersPanel.inviteWritten", { path: created.output });
            })
          }
        >
          <UserPlus className="mr-1.5 h-4 w-4" aria-hidden />
          {t("PeersPanel.inviteAction")}
        </Button>
        <p className="text-xs text-muted">{t("PeersPanel.inviteTransfer")}</p>
      </section>

      <section className="space-y-3 rounded-lg border border-border bg-surface p-4">
        <h3 className="text-sm font-semibold text-foreground">{t("PeersPanel.acceptTitle")}</h3>
        <p className="text-sm text-muted">{t("PeersPanel.acceptDetail")}</p>
        <input
          className={INPUT}
          placeholder={t("PeersPanel.acceptFilePlaceholder")}
          value={acceptDraft.invitation}
          onChange={(event) => setAcceptDraft({ ...acceptDraft, invitation: event.target.value })}
        />
        <input
          className={INPUT}
          placeholder={t("PeersPanel.acceptAliasPlaceholder")}
          value={acceptDraft.alias}
          onChange={(event) => setAcceptDraft({ ...acceptDraft, alias: event.target.value })}
        />
        <Button
          disabled={busy !== null || acceptDraft.invitation.trim() === "" || acceptDraft.alias.trim() === ""}
          onClick={() =>
            run("accept", async () => {
              const accepted = await peersAccept(acceptDraft.invitation.trim(), acceptDraft.alias.trim());
              setAcceptDraft({ invitation: "", alias: "" });
              return t("PeersPanel.acceptDone", { alias: accepted.alias, fingerprint: formatFingerprint(accepted.certificate_sha256) });
            })
          }
        >
          <Send className="mr-1.5 h-4 w-4" aria-hidden />
          {t("PeersPanel.acceptAction")}
        </Button>
      </section>

      <section className="space-y-3">
        <h3 className="text-sm font-semibold text-foreground">{t("PeersPanel.inboundTitle")}</h3>
        {inbound.length === 0 && <p className="text-sm text-muted">{t("PeersPanel.inboundEmpty")}</p>}
        {inbound.map((peer) => {
          const standing = standingSummary(peer);
          return (
            <article key={peer.device_id} className="space-y-3 rounded-lg border border-border bg-surface p-4">
              <div className="flex items-start justify-between gap-3">
                <div>
                  <p className="text-sm font-medium text-foreground">{peer.label}</p>
                  <p className="font-mono text-xs text-muted">{peer.device_id}</p>
                </div>
                <span className={standing === "revoked" ? "text-xs text-danger" : "text-xs text-muted"}>
                  {standingLabel(standing, t)}
                </span>
              </div>
              {standing === "mixed" && (
                <p className="flex items-start gap-2 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-xs text-warning">
                  <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden />
                  {t("PeersPanel.mixedPairing")}
                </p>
              )}
              <fieldset className="space-y-2">
                <legend className="text-xs font-medium uppercase tracking-wide text-muted">{t("PeersPanel.grantsLegend")}</legend>
                {PEER_GRANTS.map((grant) => (
                  <label key={grant.id} className="flex items-start gap-2 text-sm text-foreground">
                    <input
                      type="checkbox"
                      className="mt-1"
                      disabled={busy !== null || peer.state === "revoked"}
                      checked={peer.grants.includes(grant.id)}
                      onChange={() =>
                        run(`grant-${peer.device_id}`, async () => {
                          const next = toggleGrant(peer.grants, grant.id);
                          await peersGrant(peer.device_id, next);
                          return next.length === 0
                            ? t("PeersPanel.grantsCleared", { label: peer.label })
                            : t("PeersPanel.grantsSaved", { label: peer.label });
                        })
                      }
                    />
                    <span>
                      {t(grant.labelKey)}
                      <span className="block text-xs text-muted">{t(grant.detailKey)}</span>
                    </span>
                  </label>
                ))}
              </fieldset>
              <div className="flex flex-wrap items-center gap-2">
                <Button
                  variant="ghost"
                  disabled={busy !== null || peer.state === "revoked"}
                  onClick={() => setSelected(selected === peer.device_id ? null : peer.device_id)}
                >
                  {t("PeersPanel.showThreads")}
                </Button>
                {confirmRevoke === peer.device_id ? (
                  <>
                    <span className="text-xs text-danger">{t("PeersPanel.revokeConfirm")}</span>
                    <Button
                      variant="danger"
                      disabled={busy !== null}
                      onClick={() =>
                        run(`revoke-${peer.device_id}`, async () => {
                          await peersRevoke(peer.device_id, "Revoked from Settings");
                          setConfirmRevoke(null);
                          return t("PeersPanel.revoked", { label: peer.label });
                        })
                      }
                    >
                      <Trash2 className="mr-1.5 h-4 w-4" aria-hidden />
                      {t("PeersPanel.revokeConfirmAction")}
                    </Button>
                    <Button variant="ghost" onClick={() => setConfirmRevoke(null)}>
                      {t("PeersPanel.cancel")}
                    </Button>
                  </>
                ) : (
                  <Button variant="ghost" disabled={busy !== null || peer.state === "revoked"} onClick={() => setConfirmRevoke(peer.device_id)}>
                    {t("PeersPanel.revoke")}
                  </Button>
                )}
              </div>
              {selected === peer.device_id && (
                <div className="space-y-2 border-t border-border pt-3">
                  {threads.filter((thread) => thread.peer_device_id === peer.device_id).length === 0 && (
                    <p className="text-xs text-muted">{t("PeersPanel.threadsEmpty")}</p>
                  )}
                  {threads
                    .filter((thread) => thread.peer_device_id === peer.device_id)
                    .map((thread) => (
                      <div key={thread.thread_id} className="text-xs text-muted">
                        <p className="font-mono text-foreground">{thread.thread_id}</p>
                        <p>
                          {t("PeersPanel.threadSummary", { count: thread.message_count })}
                          {hasRejection(thread) ? ` · ${t("PeersPanel.threadHasRejection")}` : ""}
                        </p>
                      </div>
                    ))}
                </div>
              )}
            </article>
          );
        })}
      </section>

      <section className="space-y-3">
        <h3 className="text-sm font-semibold text-foreground">{t("PeersPanel.outboundTitle")}</h3>
        {outbound.length === 0 && <p className="text-sm text-muted">{t("PeersPanel.outboundEmpty")}</p>}
        {outbound.map((peer) => (
          <article key={peer.alias} className="space-y-1 rounded-lg border border-border bg-surface p-4">
            <p className="text-sm font-medium text-foreground">{peer.alias}</p>
            <p className="text-xs text-muted">{peer.peer_url}</p>
            <p className="flex items-center gap-1.5 font-mono text-xs text-muted">
              <Fingerprint className="h-3.5 w-3.5" aria-hidden />
              {formatFingerprint(peer.certificate_sha256)}
            </p>
            <p className="text-xs text-muted">
              {t("PeersPanel.outboundGrants", {
                grants: peer.grants.map((grant) => grantLabel(grant, t)).join(", ") || t("PeersPanel.grantsNone"),
              })}
            </p>
          </article>
        ))}
      </section>
    </div>
  );
}
