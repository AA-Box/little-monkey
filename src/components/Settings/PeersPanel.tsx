import { useCallback, useEffect, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import {
  AlertTriangle,
  Check,
  Fingerprint,
  FolderOpen,
  Loader2,
  RefreshCw,
  RotateCw,
  Send,
  Trash2,
  UserPlus,
} from "lucide-react";
import {
  type InboundPeer,
  type OutboundPeer,
  type PeerGrant,
  type PeerPresence,
  type PeerThread,
  type PeerThreadMessage,
  PEER_GRANTS,
  formatFingerprint,
  hasRejection,
  peersAccept,
  peersAcceptRotation,
  peersClear,
  peersForget,
  peersGrant,
  peersInvite,
  peersList,
  peersRevoke,
  peersRotate,
  peersStatus,
  peersThreads,
  standingSummary,
} from "../../lib/peersClient";
import { errorMessage } from "../../lib/errors";
import { useT } from "../../lib/i18n";
import { Button, StatusPill, type PillTone } from "../ui";

const INPUT =
  "w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent";
const JSON_FILTER = [{ name: "JSON", extensions: ["json"] }];

type Translate = (key: string, params?: Record<string, string | number>) => string;

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

function grantsLabel(grants: PeerGrant[], t: Translate): string {
  return grants.map((grant) => grantLabel(grant, t)).join(", ") || t("PeersPanel.grantsNone");
}

function presenceLabel(presence: PeerPresence, t: Translate): string {
  if (presence === "online") return t("PeersPanel.presenceOnline");
  if (presence === "offline") return t("PeersPanel.presenceOffline");
  return t("PeersPanel.presenceUnknown");
}

function presenceTone(presence: PeerPresence): PillTone {
  if (presence === "online") return "success";
  if (presence === "offline") return "neutral";
  return "warning";
}

function timestampLabel(timestamp: number | null, t: Translate): string {
  if (timestamp === null) return t("PeersPanel.lastSeenNever");
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(timestamp);
}

function messageKindLabel(kind: string, t: Translate): string {
  if (kind === "message") return t("PeersPanel.kindMessage");
  if (kind === "task_request") return t("PeersPanel.kindTask");
  if (kind === "result") return t("PeersPanel.kindResult");
  if (kind === "artifact") return t("PeersPanel.kindArtifact");
  return kind;
}

function messageDirectionLabel(direction: PeerThreadMessage["direction"], t: Translate): string {
  return direction === "inbound" ? t("PeersPanel.messageInbound") : t("PeersPanel.messageOutbound");
}

function dispositionLabel(disposition: PeerThreadMessage["disposition"], t: Translate): string {
  if (disposition === "accepted") return t("PeersPanel.statusAccepted");
  if (disposition === "rejected") return t("PeersPanel.statusRejected");
  return t("PeersPanel.statusDelivered");
}

function safeFileStem(value: string): string {
  return value.trim().replace(/[^a-zA-Z0-9_-]+/g, "-").replace(/^-+|-+$/g, "") || "peer";
}

interface CapabilitiesProps {
  direction: string;
  role: string;
  advertised: PeerGrant[];
  requested: PeerGrant[];
  granted: PeerGrant[];
  t: Translate;
}

function Capabilities({ direction, role, advertised, requested, granted, t }: CapabilitiesProps) {
  const entries = [
    [t("PeersPanel.direction"), direction],
    [t("PeersPanel.role"), role],
    [t("PeersPanel.advertised"), grantsLabel(advertised, t)],
    [t("PeersPanel.requested"), grantsLabel(requested, t)],
    [t("PeersPanel.granted"), grantsLabel(granted, t)],
  ];

  return (
    <dl className="grid min-w-0 grid-cols-1 gap-3 rounded-md bg-background p-3 sm:grid-cols-2 xl:grid-cols-5">
      {entries.map(([label, value]) => (
        <div key={label} className="min-w-0">
          <dt className="text-[11px] font-medium uppercase tracking-wide text-muted">{label}</dt>
          <dd className="mt-0.5 break-words text-xs text-foreground">{value}</dd>
        </div>
      ))}
    </dl>
  );
}

function ThreadHistory({ threads, t }: { threads: PeerThread[]; t: Translate }) {
  if (threads.length === 0) return <p className="text-xs text-muted">{t("PeersPanel.threadsEmpty")}</p>;

  return (
    <div className="space-y-3">
      {threads.map((thread) => (
        <section key={thread.thread_id} className="min-w-0 space-y-2 rounded-md border border-border bg-background p-3">
          <div className="flex flex-col gap-1 sm:flex-row sm:items-start sm:justify-between sm:gap-3">
            <div className="min-w-0">
              <p className="break-all font-mono text-xs text-foreground">{thread.thread_id}</p>
              <p className="text-xs text-muted">
                {t("PeersPanel.threadSummary", { count: thread.message_count })}
                {hasRejection(thread) ? ` · ${t("PeersPanel.threadHasRejection")}` : ""}
              </p>
            </div>
            <p className="shrink-0 text-xs text-muted">
              {t("PeersPanel.threadLastActivity", { time: timestampLabel(thread.last_activity_at_ms, t) })}
            </p>
          </div>
          {thread.recent.length === 0 ? (
            <p className="text-xs text-muted">{t("PeersPanel.messagesEmpty")}</p>
          ) : (
            <ol className="space-y-2" aria-label={t("PeersPanel.recentMessages")}>
              {thread.recent.slice(0, 10).map((message) => {
                const correlation = message.correlation_id ?? message.job_id;
                return (
                  <li key={message.message_id} className="min-w-0 rounded-md border border-border/70 bg-surface p-2.5 text-xs">
                    <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-foreground">
                      <span className="font-medium">{messageKindLabel(message.kind, t)}</span>
                      <span className="text-muted">{messageDirectionLabel(message.direction, t)}</span>
                      <StatusPill tone={message.disposition === "rejected" ? "danger" : message.disposition === "delivered" ? "success" : "neutral"}>
                        {dispositionLabel(message.disposition, t)}
                      </StatusPill>
                    </div>
                    <dl className="mt-2 grid min-w-0 gap-1 text-muted sm:grid-cols-2">
                      <div className="min-w-0">
                        <dt className="inline font-medium">{t("PeersPanel.correlation")}: </dt>
                        <dd className="inline break-all font-mono">{correlation ?? t("PeersPanel.notSupplied")}</dd>
                      </div>
                      <div className="min-w-0">
                        <dt className="inline font-medium">{t("PeersPanel.messageId")}: </dt>
                        <dd className="inline break-all font-mono">{message.message_id}</dd>
                      </div>
                    </dl>
                    {message.rejection && <p className="mt-1 break-words text-danger">{message.rejection}</p>}
                  </li>
                );
              })}
            </ol>
          )}
        </section>
      ))}
    </div>
  );
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
  const [inviteDraft, setInviteDraft] = useState<{ label: string; grants: PeerGrant[] }>({
    label: "",
    grants: ["message"],
  });
  const [acceptDraft, setAcceptDraft] = useState({ invitation: "", alias: "" });
  const [confirmRevoke, setConfirmRevoke] = useState<string | null>(null);
  const [confirmClear, setConfirmClear] = useState<string | null>(null);
  const [confirmForget, setConfirmForget] = useState<string | null>(null);

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

  const chooseInvitation = useCallback(async () => {
    setBusy("choose-invitation");
    setError(null);
    try {
      const invitation = await open({ multiple: false, directory: false, filters: JSON_FILTER });
      if (typeof invitation === "string") setAcceptDraft((draft) => ({ ...draft, invitation }));
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(null);
    }
  }, []);

  const toggleGrant = (grants: PeerGrant[], grant: PeerGrant): PeerGrant[] =>
    grants.includes(grant) ? grants.filter((value) => value !== grant) : [...grants, grant];

  if (inbound === null) {
    return (
      <div role="status" aria-live="polite" className="flex items-center gap-2 text-sm text-muted">
        <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
        {t("PeersPanel.loading")}
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <header className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="space-y-1">
          <h2 className="text-base font-semibold text-foreground">{t("PeersPanel.title")}</h2>
          <p className="max-w-3xl text-sm text-muted">{t("PeersPanel.intro")}</p>
        </div>
        <Button
          size="sm"
          className="shrink-0 self-start"
          disabled={busy !== null}
          onClick={() => void run("refresh", async () => t("PeersPanel.refreshed"))}
        >
          <RefreshCw className={`h-3.5 w-3.5 ${busy === "refresh" ? "animate-spin" : ""}`} aria-hidden />
          {t("PeersPanel.refresh")}
        </Button>
      </header>

      {error && (
        <p role="alert" className="flex items-start gap-2 rounded-md border border-danger/40 bg-danger/10 px-3 py-2 text-sm text-danger">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
          {error}
        </p>
      )}
      {notice && (
        <p role="status" aria-live="polite" className="flex items-start gap-2 rounded-md border border-success/40 bg-success/10 px-3 py-2 text-sm text-success">
          <Check className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
          {notice}
        </p>
      )}

      <div className="grid gap-4 xl:grid-cols-2">
        <section className="space-y-3 rounded-lg border border-border bg-surface p-4">
          <h3 className="text-sm font-semibold text-foreground">{t("PeersPanel.inviteTitle")}</h3>
          <p className="text-sm text-muted">{t("PeersPanel.inviteDetail")}</p>
          <div className="space-y-1.5">
            <label htmlFor="peer-invite-label" className="text-xs font-medium text-foreground">
              {t("PeersPanel.inviteLabel")}
            </label>
            <input
              id="peer-invite-label"
              className={INPUT}
              placeholder={t("PeersPanel.inviteLabelPlaceholder")}
              value={inviteDraft.label}
              onChange={(event) => setInviteDraft({ ...inviteDraft, label: event.target.value })}
            />
          </div>
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
            disabled={busy !== null || inviteDraft.label.trim() === "" || inviteDraft.grants.length === 0}
            onClick={() =>
              void run("invite", async () => {
                const output = await save({ defaultPath: "little-monkey-peer-invitation.json", filters: JSON_FILTER });
                if (!output) return null;
                const created = await peersInvite(inviteDraft.label.trim(), inviteDraft.grants, 60, output);
                setInviteDraft({ label: "", grants: ["message"] });
                return t("PeersPanel.inviteWritten", { path: created.output });
              })
            }
          >
            <UserPlus className="h-4 w-4" aria-hidden />
            {t("PeersPanel.inviteAction")}
          </Button>
          <p className="text-xs text-muted">{t("PeersPanel.inviteTransfer")}</p>
        </section>

        <section className="space-y-3 rounded-lg border border-border bg-surface p-4">
          <h3 className="text-sm font-semibold text-foreground">{t("PeersPanel.acceptTitle")}</h3>
          <p className="text-sm text-muted">{t("PeersPanel.acceptDetail")}</p>
          <div className="space-y-1.5">
            <label htmlFor="peer-accept-alias" className="text-xs font-medium text-foreground">
              {t("PeersPanel.acceptAlias")}
            </label>
            <input
              id="peer-accept-alias"
              className={INPUT}
              placeholder={t("PeersPanel.acceptAliasPlaceholder")}
              value={acceptDraft.alias}
              onChange={(event) => setAcceptDraft({ ...acceptDraft, alias: event.target.value })}
            />
          </div>
          <div className="space-y-1.5">
            <label htmlFor="peer-accept-file" className="text-xs font-medium text-foreground">
              {t("PeersPanel.acceptFile")}
            </label>
            <div className="flex flex-col gap-2 sm:flex-row">
              <input
                id="peer-accept-file"
                className={`${INPUT} min-w-0 flex-1 font-mono text-xs`}
                value={acceptDraft.invitation}
                placeholder={t("PeersPanel.noFileSelected")}
                readOnly
              />
              <Button disabled={busy !== null} onClick={() => void chooseInvitation()}>
                <FolderOpen className="h-4 w-4" aria-hidden />
                {t("PeersPanel.chooseFile")}
              </Button>
            </div>
          </div>
          <Button
            disabled={busy !== null || acceptDraft.invitation === "" || acceptDraft.alias.trim() === ""}
            onClick={() =>
              void run("accept", async () => {
                const accepted = await peersAccept(acceptDraft.invitation, acceptDraft.alias.trim());
                setAcceptDraft({ invitation: "", alias: "" });
                return t("PeersPanel.acceptDone", {
                  alias: accepted.alias,
                  fingerprint: formatFingerprint(accepted.certificate_sha256),
                });
              })
            }
          >
            <Send className="h-4 w-4" aria-hidden />
            {t("PeersPanel.acceptAction")}
          </Button>
        </section>
      </div>

      <section className="space-y-3">
        <h3 className="text-sm font-semibold text-foreground">{t("PeersPanel.inboundTitle")}</h3>
        {inbound.length === 0 && <p className="text-sm text-muted">{t("PeersPanel.inboundEmpty")}</p>}
        {inbound.map((peer, index) => {
          const standing = standingSummary(peer);
          const peerThreads = threads.filter((thread) => thread.peer_device_id === peer.device_id).slice(0, 20);
          const threadsId = `peer-threads-${index}`;
          return (
            <article key={peer.device_id} className="min-w-0 space-y-3 overflow-hidden rounded-lg border border-border bg-surface p-4">
              <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
                <div className="min-w-0">
                  <p className="break-words text-sm font-medium text-foreground">{peer.label}</p>
                  <p className="break-all font-mono text-xs text-muted">{peer.device_id}</p>
                  <p className="mt-1 text-xs text-muted">
                    {t("PeersPanel.lastSeen", { time: timestampLabel(peer.last_seen_at_ms, t) })} · {t("PeersPanel.keyGeneration", { generation: peer.secret_generation })}
                  </p>
                </div>
                <div className="flex shrink-0 flex-wrap items-center gap-2">
                  <StatusPill tone={presenceTone(peer.presence)}>{presenceLabel(peer.presence, t)}</StatusPill>
                  <span className={standing === "revoked" ? "text-xs text-danger" : "text-xs text-muted"}>
                    {standingLabel(standing, t)}
                  </span>
                </div>
              </div>
              <Capabilities
                direction={t("PeersPanel.directionInbound")}
                role={peer.peer_only ? t("PeersPanel.rolePeer") : t("PeersPanel.roleMixed")}
                advertised={peer.advertised_grants}
                requested={peer.requested_grants}
                granted={peer.grants}
                t={t}
              />
              {standing === "mixed" && (
                <p className="flex items-start gap-2 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-xs text-warning">
                  <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden />
                  {t("PeersPanel.mixedPairing")}
                </p>
              )}
              <fieldset className="space-y-2">
                <legend className="text-xs font-medium uppercase tracking-wide text-muted">{t("PeersPanel.grantsLegend")}</legend>
                <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-3">
                  {PEER_GRANTS.map((grant) => (
                    <label key={grant.id} className="flex items-start gap-2 text-sm text-foreground">
                      <input
                        type="checkbox"
                        className="mt-1"
                        disabled={busy !== null || peer.state === "revoked"}
                        checked={peer.grants.includes(grant.id)}
                        onChange={() =>
                          void run(`grant-${peer.device_id}`, async () => {
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
                </div>
              </fieldset>
              <div className="flex flex-wrap items-center gap-2">
                <Button
                  variant="ghost"
                  disabled={busy !== null}
                  aria-expanded={selected === peer.device_id}
                  aria-controls={threadsId}
                  onClick={() => setSelected(selected === peer.device_id ? null : peer.device_id)}
                >
                  {t("PeersPanel.showThreads", { count: peerThreads.length })}
                </Button>
                <Button
                  variant="ghost"
                  disabled={busy !== null || peer.state === "revoked"}
                  onClick={() =>
                    void run(`rotate-${peer.device_id}`, async () => {
                      const output = await save({
                        defaultPath: `${safeFileStem(peer.label)}-peer-rotation.json`,
                        filters: JSON_FILTER,
                      });
                      if (!output) return null;
                      const rotated = await peersRotate(peer.device_id, output);
                      return t("PeersPanel.rotated", {
                        label: peer.label,
                        generation: rotated.secret_generation,
                        path: rotated.output,
                      });
                    })
                  }
                >
                  <RotateCw className="h-3.5 w-3.5" aria-hidden />
                  {t("PeersPanel.rotate")}
                </Button>
                {confirmClear === peer.device_id ? (
                  <div className="flex flex-wrap items-center gap-2" role="group" aria-label={t("PeersPanel.clearConfirmGroup", { label: peer.label })}>
                    <span className="text-xs text-danger">{t("PeersPanel.clearConfirm")}</span>
                    <Button
                      autoFocus
                      variant="danger"
                      disabled={busy !== null}
                      onClick={() =>
                        void run(`clear-${peer.device_id}`, async () => {
                          const cleared = await peersClear(peer.device_id);
                          setConfirmClear(null);
                          setSelected(null);
                          return t("PeersPanel.cleared", {
                            label: peer.label,
                            count: cleared.threads_removed,
                          });
                        })
                      }
                    >
                      <Trash2 className="h-3.5 w-3.5" aria-hidden />
                      {t("PeersPanel.clearConfirmAction")}
                    </Button>
                    <Button variant="ghost" disabled={busy !== null} onClick={() => setConfirmClear(null)}>
                      {t("PeersPanel.cancel")}
                    </Button>
                  </div>
                ) : (
                  <Button variant="ghost" disabled={busy !== null} onClick={() => setConfirmClear(peer.device_id)}>
                    {t("PeersPanel.clear")}
                  </Button>
                )}
                {confirmRevoke === peer.device_id ? (
                  <div className="flex flex-wrap items-center gap-2" role="group" aria-label={t("PeersPanel.revokeConfirmGroup", { label: peer.label })}>
                    <span className="text-xs text-danger">{t("PeersPanel.revokeConfirm")}</span>
                    <Button
                      autoFocus
                      variant="danger"
                      disabled={busy !== null}
                      onClick={() =>
                        void run(`revoke-${peer.device_id}`, async () => {
                          await peersRevoke(peer.device_id, "Revoked from Settings");
                          setConfirmRevoke(null);
                          return t("PeersPanel.revoked", { label: peer.label });
                        })
                      }
                    >
                      <Trash2 className="h-4 w-4" aria-hidden />
                      {t("PeersPanel.revokeConfirmAction")}
                    </Button>
                    <Button variant="ghost" disabled={busy !== null} onClick={() => setConfirmRevoke(null)}>
                      {t("PeersPanel.cancel")}
                    </Button>
                  </div>
                ) : (
                  <Button variant="ghost" disabled={busy !== null || peer.state === "revoked"} onClick={() => setConfirmRevoke(peer.device_id)}>
                    {t("PeersPanel.revoke")}
                  </Button>
                )}
              </div>
              {selected === peer.device_id && (
                <div id={threadsId} className="space-y-2 border-t border-border pt-3">
                  <ThreadHistory threads={peerThreads} t={t} />
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
          <article key={peer.alias} className="min-w-0 space-y-3 overflow-hidden rounded-lg border border-border bg-surface p-4">
            <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
              <div className="min-w-0">
                <p className="break-words text-sm font-medium text-foreground">{peer.alias}</p>
                <p className="break-all text-xs text-muted">{peer.peer_url}</p>
                <p className="break-all font-mono text-xs text-muted">{peer.peer_id}</p>
                <p className="mt-1 text-xs text-muted">
                  {t("PeersPanel.lastSeen", { time: timestampLabel(peer.last_seen_at_ms, t) })} · {t("PeersPanel.keyGeneration", { generation: peer.secret_generation })}
                </p>
              </div>
              <StatusPill tone={presenceTone(peer.presence)}>{presenceLabel(peer.presence, t)}</StatusPill>
            </div>
            <p className="flex min-w-0 items-start gap-1.5 break-all font-mono text-xs text-muted">
              <Fingerprint className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden />
              <span>
                <span className="sr-only">{t("PeersPanel.certificateFingerprint")}: </span>
                {formatFingerprint(peer.certificate_sha256)}
              </span>
            </p>
            <Capabilities
              direction={t("PeersPanel.directionOutbound")}
              role={t("PeersPanel.rolePeer")}
              advertised={peer.advertised_grants}
              requested={peer.requested_grants}
              granted={peer.grants}
              t={t}
            />
            <p className="text-xs text-muted">
              {t("PeersPanel.outboundGrants", { grants: grantsLabel(peer.grants, t) })}
            </p>
            <div className="flex flex-wrap items-center gap-2">
              <Button
                variant="ghost"
                disabled={busy !== null}
                aria-label={t("PeersPanel.refreshPeerStatusLabel", { alias: peer.alias })}
                onClick={() =>
                  void run(`status-${peer.alias}`, async () => {
                    const status = await peersStatus(peer.alias);
                    return t("PeersPanel.statusChecked", {
                      alias: peer.alias,
                      presence: presenceLabel(status.presence, t),
                    });
                  })
                }
              >
                <RefreshCw className={`h-3.5 w-3.5 ${busy === `status-${peer.alias}` ? "animate-spin" : ""}`} aria-hidden />
                {t("PeersPanel.refreshStatus")}
              </Button>
              <Button
                variant="ghost"
                disabled={busy !== null}
                onClick={() =>
                  void run(`accept-rotation-${peer.alias}`, async () => {
                    const bundle = await open({ multiple: false, directory: false, filters: JSON_FILTER });
                    if (typeof bundle !== "string") return null;
                    const accepted = await peersAcceptRotation(bundle, peer.alias);
                    return t("PeersPanel.rotationAccepted", {
                      alias: accepted.alias,
                      generation: accepted.secret_generation,
                      fingerprint: formatFingerprint(accepted.certificate_sha256),
                    });
                  })
                }
              >
                <RotateCw className="h-3.5 w-3.5" aria-hidden />
                {t("PeersPanel.acceptRotation")}
              </Button>
              {confirmForget === peer.alias ? (
                <div className="flex flex-wrap items-center gap-2" role="group" aria-label={t("PeersPanel.forgetConfirmGroup", { alias: peer.alias })}>
                  <span className="text-xs text-danger">{t("PeersPanel.forgetConfirm")}</span>
                  <Button
                    autoFocus
                    variant="danger"
                    disabled={busy !== null}
                    onClick={() =>
                      void run(`forget-${peer.alias}`, async () => {
                        await peersForget(peer.alias);
                        setConfirmForget(null);
                        return t("PeersPanel.forgotten", { alias: peer.alias });
                      })
                    }
                  >
                    <Trash2 className="h-3.5 w-3.5" aria-hidden />
                    {t("PeersPanel.forgetConfirmAction")}
                  </Button>
                  <Button variant="ghost" disabled={busy !== null} onClick={() => setConfirmForget(null)}>
                    {t("PeersPanel.cancel")}
                  </Button>
                </div>
              ) : (
                <Button variant="ghost" disabled={busy !== null} onClick={() => setConfirmForget(peer.alias)}>
                  {t("PeersPanel.forget")}
                </Button>
              )}
            </div>
          </article>
        ))}
      </section>
    </div>
  );
}
