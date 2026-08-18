import { useCallback, useEffect, useState } from "react";
import { RefreshCw } from "lucide-react";

import {
  SOURCE_LABELS,
  ingressTurns,
  turnFailureReason,
  turnStatus,
  type ConversationSource,
  type IngressTurn,
} from "../../lib/ingressClient";
import { Button } from "../ui";
import { errorMessage } from "../../lib/errors";
import { useT } from "../../lib/i18n";

/**
 * What arrived from outside, across every origin, and what it became.
 *
 * One list rather than one per subsystem, because the question is the same
 * whether the turn came from a Telegram message, a phone call, a paired
 * device, a peer node, the microphone or this window: did Little Monkey take
 * it, and did it run? The six origins are the ones the durable contract
 * defines, and every row here is a real production turn — there is no other
 * way for a conversation to reach the queue.
 *
 * Deliberately not shown: what was said. This is a status surface, not a
 * transcript, and the backend listing has no field for message text or for a
 * credential.
 */

const STATUS_TONE: Record<ReturnType<typeof turnStatus>, string> = {
  waiting: "text-muted",
  running: "text-warning",
  done: "text-success",
  failed: "text-danger",
};

const SOURCES: ConversationSource[] = [
  "desktop",
  "mobile",
  "messaging_channel",
  "peer",
  "voice",
  "telephone",
];

function shortDigest(digest: string | null): string {
  return digest ? digest.slice(0, 12) : "—";
}

export function IngressTurnsSection() {
  const { t } = useT();
  const [turns, setTurns] = useState<IngressTurn[] | null>(null);
  const [source, setSource] = useState<ConversationSource | "">("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setBusy(true);
    try {
      const listed = await ingressTurns(source === "" ? null : source, 25);
      setTurns(listed.turns);
      setError(null);
    } catch (reason) {
      setError(errorMessage(reason));
      setTurns([]);
    } finally {
      setBusy(false);
    }
  }, [source]);

  useEffect(() => { void load(); }, [load]);

  return (
    <section className="rounded-lg border border-border bg-surface p-4">
      <div className="flex items-center justify-between gap-2">
        <h4 className="text-sm font-semibold">{t("IngressTurns.title")}</h4>
        <div className="flex items-center gap-2">
          <select
            aria-label={t("IngressTurns.filter")}
            className="rounded-md border border-border bg-background px-2 py-1 text-xs text-foreground"
            value={source}
            onChange={(event) => setSource(event.target.value as ConversationSource | "")}
          >
            <option value="">{t("IngressTurns.allSources")}</option>
            {SOURCES.map((entry) => (
              <option key={entry} value={entry}>{SOURCE_LABELS[entry]}</option>
            ))}
          </select>
          <Button size="sm" disabled={busy} onClick={() => void load()}>
            <RefreshCw size={14} />{t("IngressTurns.refresh")}
          </Button>
        </div>
      </div>
      <p className="mt-1 text-xs text-muted">{t("IngressTurns.intro")}</p>

      {error && <p role="alert" className="mt-2 text-xs text-danger">{error}</p>}

      {turns !== null && turns.length === 0 && !error && (
        <p className="mt-3 text-xs text-faint">{t("IngressTurns.empty")}</p>
      )}

      <ul className="mt-3 flex flex-col gap-2">
        {(turns ?? []).map((turn) => {
          const status = turnStatus(turn);
          const reason = turnFailureReason(turn);
          return (
            <li key={turn.ingress_id} className="rounded-md border border-border bg-background p-2 text-xs">
              <div className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
                <span className="font-medium">{SOURCE_LABELS[turn.source]}</span>
                <span className="text-muted">{turn.account_label ?? turn.source_account_id}</span>
                <span className={`ml-auto font-medium ${STATUS_TONE[status]}`}>
                  {t(`IngressTurns.status.${status}`)}
                </span>
              </div>
              <dl className="mt-1 grid grid-cols-2 gap-x-3 gap-y-0.5 text-faint sm:grid-cols-3">
                <div><dt className="inline">{t("IngressTurns.event")}: </dt><dd className="inline">{turn.source_event_id}</dd></div>
                <div><dt className="inline">{t("IngressTurns.state")}: </dt><dd className="inline">{turn.state}</dd></div>
                <div><dt className="inline">{t("IngressTurns.attempts")}: </dt><dd className="inline">{turn.attempts}</dd></div>
                <div><dt className="inline">{t("IngressTurns.job")}: </dt><dd className="inline">{turn.job_id ?? "—"}</dd></div>
                <div><dt className="inline">{t("IngressTurns.run")}: </dt><dd className="inline">{turn.run_id ?? "—"}</dd></div>
                <div>
                  <dt className="inline">{t("IngressTurns.snapshot")}: </dt>
                  <dd className="inline">
                    {turn.execution_version === null ? "—" : `v${turn.execution_version} ${shortDigest(turn.execution_digest)}`}
                  </dd>
                </div>
                {turn.mutation_required && (
                  <div>
                    <dt className="inline">{t("IngressTurns.contract")}: </dt>
                    <dd className="inline">
                      {turn.mutation_state === null
                        ? t("IngressTurns.contractPending")
                        : t(`IngressTurns.contractState.${turn.mutation_state}`)}
                    </dd>
                  </div>
                )}
                {turn.continuation_kind !== null && (
                  <div>
                    <dt className="inline">{t("IngressTurns.continuation")}: </dt>
                    <dd className="inline">
                      {t(`IngressTurns.continuationKind.${turn.continuation_kind}`)}
                      {` #${turn.continuation_attempt} · ${turn.parent_ingress_id ?? "—"}`}
                    </dd>
                  </div>
                )}
              </dl>
              {turn.mutation_detail && <p className="mt-1 text-muted">{turn.mutation_detail}</p>}
              {reason && <p className="mt-1 text-danger">{reason}</p>}
            </li>
          );
        })}
      </ul>
    </section>
  );
}
