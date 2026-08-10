import { useCallback, useEffect, useState } from "react";
import { RefreshCw, Server } from "lucide-react";

import {
  remoteNodeList,
  remoteNodeRefresh,
  remotePlacements,
  remotePlacementSync,
  type RemoteNodeRow,
  type RemotePlacementRow,
} from "../../lib/daemonClient";
import { errorMessage } from "../../lib/errors";
import { useT } from "../../lib/i18n";
import { formatTimestamp } from "../../lib/format";
import { Button, StatusPill, type PillTone } from "../ui";

/**
 * The nodes this machine may place work on, and the runs it has placed there
 * (roadmap K17 S1/S4/S5).
 *
 * Read-only on purpose. Placing a run means authoring a frozen `RunSpec`, and
 * a button that quietly composed one would be inventing a policy the operator
 * never wrote — the exact failure K17 S3 exists to prevent. The two actions
 * here are the two that only ever *ask*: re-describe a node, and re-read the
 * placements on it.
 *
 * Every number shown is the daemon's own answer, including `liveness`: the
 * silence thresholds live in `node_placement.rs` and are deliberately not
 * recomputed from `last_seen_at_ms` here, so there is one implementation of
 * "how long is too long" rather than two that can disagree.
 */
export function PlacedNodesPanel() {
  const { t } = useT();
  const [nodes, setNodes] = useState<RemoteNodeRow[]>([]);
  const [placements, setPlacements] = useState<RemotePlacementRow[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [nodeList, placed] = await Promise.all([remoteNodeList(), remotePlacements()]);
      setNodes(nodeList.nodes ?? []);
      setPlacements(placed.placements ?? []);
      setError(null);
    } catch (cause) {
      // A machine with no background runner installed is the ordinary case, not
      // a failure worth a red banner over the run list — it is reported inline
      // and the section stays collapsed and empty.
      setError(errorMessage(cause));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function act(label: string, action: () => Promise<unknown>) {
    setBusy(label);
    try {
      await action();
      await load();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  }

  if (nodes.length === 0 && placements.length === 0 && !error) {
    return null;
  }

  return (
    <details className="border-b border-border px-4 py-2 text-xs">
      <summary className="cursor-pointer select-none font-medium text-muted hover:text-foreground">
        <Server size={13} className="mr-1 inline" />
        {t("RunCenter.nodesTitle")} ({nodes.length})
      </summary>

      <div className="mt-2 flex flex-wrap gap-2">
        <Button
          size="sm"
          disabled={busy !== null}
          onClick={() => void act("describe", () => remoteNodeRefresh())}
        >
          <RefreshCw size={12} className={busy === "describe" ? "animate-spin" : ""} />{" "}
          {t("RunCenter.nodesDescribe")}
        </Button>
        <Button size="sm" disabled={busy !== null} onClick={() => void act("sync", remotePlacementSync)}>
          {t("RunCenter.placementsSync")}
        </Button>
      </div>

      {error && (
        <p role="alert" className="mt-2 rounded-md border border-danger/30 bg-danger-soft p-2 text-danger">
          {error}
        </p>
      )}

      {nodes.length > 0 && (
        <ul className="mt-2 space-y-1">
          {nodes.map((node) => (
            <li
              key={node.alias}
              className="flex flex-wrap items-center gap-2 rounded-md border border-border bg-surface p-2"
            >
              <StatusPill tone={livenessTone(node.liveness)}>{node.liveness}</StatusPill>
              <span className="font-medium">{node.node_name}</span>
              <span className="text-faint">{node.alias}</span>
              <span className="text-muted">
                {t("RunCenter.nodeResidency")}: {node.residency}
              </span>
              <span className="text-muted">
                {t("RunCenter.nodeQueue")}: {node.queue_depth}/{node.queue_capacity}
                {node.accepting ? "" : ` · ${t("RunCenter.nodeRefusing")}`}
              </span>
              {node.last_seen_at_ms !== null && (
                <time className="text-faint" dateTime={new Date(node.last_seen_at_ms).toISOString()}>
                  {formatTimestamp(node.last_seen_at_ms)}
                </time>
              )}
            </li>
          ))}
        </ul>
      )}

      {placements.length > 0 && (
        <ul className="mt-2 space-y-1">
          {placements.map((placement) => (
            <li key={placement.submitted_run_id} className="rounded-md border border-border bg-surface p-2">
              <div className="flex flex-wrap items-center gap-2">
                <StatusPill tone={placementTone(placement.state)}>{placement.state}</StatusPill>
                <span className="font-mono">{placement.submitted_run_id}</span>
                <span className="text-muted">
                  {t("RunCenter.placementOn")} {placement.alias} · {placement.residency}
                </span>
                {/* The record says why, not only where — `select_node`'s deciding key. */}
                <span className="text-faint">{placement.deciding_key}</span>
                {placement.attempt > 1 && (
                  <span className="text-faint">
                    {t("RunCenter.placementAttempt")} {placement.attempt}
                  </span>
                )}
              </div>
              {placement.last_error && <p className="mt-1 text-faint">{placement.last_error}</p>}
            </li>
          ))}
        </ul>
      )}
    </details>
  );
}

export function livenessTone(liveness: string): PillTone {
  if (liveness === "alive") return "success";
  return liveness === "stale" ? "warning" : "danger";
}

export function placementTone(state: string): PillTone {
  if (state === "succeeded") return "success";
  if (state === "failed" || state === "lost") return "danger";
  // `accepted`/`running`/`cancelled` are all "nothing to act on here".
  return "neutral";
}
