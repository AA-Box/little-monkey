import { useEffect, useMemo } from "react";
import { RefreshCw, ShieldCheck } from "lucide-react";
import { StatusPill, type PillTone } from "../../ui";
import type { M3CompatibilityMatrixRow, M3CompatibilityStatus } from "../../../lib/runtimeHubClient";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import { BusyButton, ErrorNotice, SectionHeading, labelize } from "./RuntimeHubShared";

const STATUS_TONE: Record<M3CompatibilityStatus, PillTone> = {
  pass: "success",
  unsupported: "warning",
  fail: "danger",
};

const STATUS_LABEL: Record<M3CompatibilityStatus, string> = {
  pass: "Pass",
  unsupported: "Unsupported",
  fail: "Fail",
};

export function groupByRuntime(rows: M3CompatibilityMatrixRow[]): Map<string, M3CompatibilityMatrixRow[]> {
  const groups = new Map<string, M3CompatibilityMatrixRow[]>();
  for (const row of rows) {
    const bucket = groups.get(row.runtimeId);
    if (bucket) bucket.push(row);
    else groups.set(row.runtimeId, [row]);
  }
  return groups;
}

export function RuntimeHubCompatibilityMatrix() {
  const matrix = useRuntimeHubStore((state) => state.compatibilityMatrix);
  const refresh = useRuntimeHubStore((state) => state.refreshCompatibilityMatrix);
  const busy = useRuntimeHubStore((state) => state.busy["compatibility-matrix"]);
  const error = useRuntimeHubStore((state) => state.errors["compatibility-matrix"]);

  useEffect(() => {
    if (!matrix) void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const groups = useMemo(() => groupByRuntime(matrix?.rows ?? []), [matrix]);
  const counts = useMemo(() => {
    const totals: Record<M3CompatibilityStatus, number> = { pass: 0, unsupported: 0, fail: 0 };
    for (const row of matrix?.rows ?? []) totals[row.status] += 1;
    return totals;
  }, [matrix]);

  return (
    <div role="tabpanel" id="runtime-hub-panel-compatibility" aria-labelledby="runtime-hub-tab-compatibility" className="flex flex-col gap-5">
      <SectionHeading
        title="OpenAI / Ollama API compatibility matrix"
        description="Per-route, per-backend, per-model compatibility derived from live runtime and model capability state. Regressions in the actual wire behavior are caught by the m3_compatibility_harness integration test suite; this table shows what is currently configured to work."
        action={
          <BusyButton type="button" busy={busy} onClick={() => void refresh()}>
            <RefreshCw size={15} aria-hidden="true" /> Run compatibility check
          </BusyButton>
        }
      />

      <ErrorNotice message={error} />

      {matrix && (
        <div className="flex flex-wrap items-center gap-2 text-xs text-muted">
          <ShieldCheck size={14} aria-hidden="true" />
          <span>Generated {new Date(matrix.generatedAtMs).toLocaleString()}</span>
          <StatusPill tone="success">{counts.pass} pass</StatusPill>
          <StatusPill tone="warning">{counts.unsupported} unsupported</StatusPill>
          <StatusPill tone="danger">{counts.fail} fail</StatusPill>
        </div>
      )}

      {!matrix && !busy && !error && (
        <p className="text-sm text-muted">No compatibility data yet. Run the compatibility check above.</p>
      )}

      {[...groups.entries()].map(([runtimeId, rows]) => (
        <section key={runtimeId} className="rounded-lg border border-border bg-background p-4">
          <h4 className="mb-3 text-sm font-semibold text-foreground">
            {runtimeId} <span className="font-normal text-muted">({labelize(rows[0]?.backend ?? "")})</span>
          </h4>
          <div className="overflow-x-auto">
            <table className="w-full min-w-[640px] border-collapse text-left text-xs">
              <thead>
                <tr className="border-b border-border text-muted">
                  <th className="py-1.5 pr-3 font-medium">Method</th>
                  <th className="py-1.5 pr-3 font-medium">Route</th>
                  <th className="py-1.5 pr-3 font-medium">Model</th>
                  <th className="py-1.5 pr-3 font-medium">Status</th>
                  <th className="py-1.5 font-medium">Reason</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((row, index) => (
                  <tr key={`${row.method}-${row.route}-${row.modelId ?? "runtime"}-${index}`} className="border-b border-border/60 last:border-0">
                    <td className="py-1.5 pr-3 font-mono text-foreground">{row.method}</td>
                    <td className="py-1.5 pr-3 font-mono text-foreground">{row.route}</td>
                    <td className="py-1.5 pr-3 text-muted">{row.modelId ?? "—"}</td>
                    <td className="py-1.5 pr-3">
                      <StatusPill tone={STATUS_TONE[row.status]}>{STATUS_LABEL[row.status]}</StatusPill>
                    </td>
                    <td className="py-1.5 text-muted">{row.reason}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </section>
      ))}
    </div>
  );
}

export default RuntimeHubCompatibilityMatrix;
