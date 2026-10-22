import { useEffect, useState } from "react";
import { RefreshCw } from "lucide-react";
import { Button } from "../../ui";
import { runtimeHubClient, type M3LoadRecoveryRecord } from "../../../lib/runtimeHubClient";
import { errorMessage } from "../../../lib/errors";
import { ErrorNotice, formatDate } from "./RuntimeHubShared";

/** Native-only bounded evidence; the backend never persists prompt text or logs. */
export function RuntimeHubLoadRecoveryHistory({ runtimeId, loading }: { runtimeId: string; loading: boolean }) {
  const [records, setRecords] = useState<M3LoadRecoveryRecord[]>([]);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [refreshNumber, setRefreshNumber] = useState(0);

  useEffect(() => {
    let active = true;
    void runtimeHubClient.runtimeLoadRecoveryHistory().then((all) => {
      if (!active) return;
      setRecords(all.filter((record) => record.runtimeId === runtimeId).slice(-5).reverse());
      setHistoryError(null);
    }).catch((error: unknown) => {
      if (active) setHistoryError(errorMessage(error));
    });
    return () => { active = false; };
  }, [runtimeId, loading, refreshNumber]);

  return (
    <section className="mt-4 rounded-md border border-border bg-surface-2 p-3" aria-label="Model-load recovery history">
      <div className="flex items-center justify-between gap-3">
        <p className="text-xs font-medium text-foreground">Model-load attempts</p>
        <Button type="button" className="min-h-9" onClick={() => setRefreshNumber((value) => value + 1)}>
          <RefreshCw size={13} aria-hidden="true" /> Refresh
        </Button>
      </div>
      {historyError && <ErrorNotice message={historyError} />}
      {!historyError && records.length === 0 && <p className="mt-2 text-xs text-muted">No recorded loads yet.</p>}
      {records.map((record, index) => (
        <div key={`${record.atMs}-${record.assetId}-${index}`} className="mt-2 border-t border-border pt-2 text-xs text-muted">
          <p className="break-all text-foreground">{record.assetId} · {formatDate(record.atMs)} · {record.recovered ? "Recovered after OOM" : record.attempts[record.attempts.length - 1]?.outcome === "success" ? "Loaded normally" : "Load failed"}</p>
          <ol className="mt-1 list-inside list-decimal space-y-1">
            {record.attempts.map((attempt) => (
              <li key={attempt.ordinal}>
                {attempt.outcome.replace(/_/g, " ")} · context {attempt.contextTokens.toLocaleString()} · GPU layers {attempt.gpuLayers === -1 ? "runtime default" : attempt.gpuLayers}
              </li>
            ))}
          </ol>
        </div>
      ))}
    </section>
  );
}

