import { useEffect, useMemo, useState } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import { Download, Eye, RefreshCw, ShieldCheck } from "lucide-react";
import type { RuntimeTraceRecord } from "../../../lib/runtimeHubClient";
import { groupTracesByModel, serializeSupportBundle, supportBundleFileName } from "../../../lib/runtimeTelemetry";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import { StatusPill } from "../../ui";
import { BusyButton, CONTROL_CLASS, ErrorNotice, JsonView, SectionHeading, SuccessNotice } from "./RuntimeHubShared";
import { errorMessage } from "../../../lib/errors";
import { formatBytes, formatDuration } from "../../../lib/format";



function TraceCard({ trace }: { trace: RuntimeTraceRecord }) {
  const when = new Date(trace.recordedAtMs).toLocaleString();
  return (
    <li className="rounded-lg border border-border bg-background p-3 text-xs">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <StatusPill tone={trace.outcome === "success" ? "success" : "danger"}>{trace.outcome}</StatusPill>
          <span className="font-medium text-foreground">
            {trace.event.kind === "load" ? "Load" : "Request"} · {trace.runtimeId}
          </span>
        </div>
        <time className="text-faint">{when}</time>
      </div>

      {trace.event.kind === "load" ? (
        <dl className="mt-2 grid gap-2 sm:grid-cols-2 lg:grid-cols-4">
          <div><dt className="text-faint">Duration</dt><dd className="font-medium text-foreground">{formatDuration(trace.event.timing.durationMs)}</dd></div>
          {trace.event.offload && (
            <>
              <div><dt className="text-faint">Accelerator</dt><dd className="font-medium text-foreground">{trace.event.offload.accelerator}</dd></div>
              <div><dt className="text-faint">GPU layers</dt><dd className="font-medium text-foreground">{trace.event.offload.gpuLayers}/{trace.event.offload.estimatedTotalLayers}</dd></div>
              <div><dt className="text-faint">Context tokens</dt><dd className="font-medium text-foreground">{trace.event.offload.contextTokens.toLocaleString()}</dd></div>
              <div><dt className="text-faint">CPU spill layers</dt><dd className="font-medium text-foreground">{trace.event.offload.cpuSpillLayers}</dd></div>
              <div><dt className="text-faint">Projector placement</dt><dd className="font-medium text-foreground">{trace.event.offload.projectorPlacement}</dd></div>
            </>
          )}
          {trace.event.memory && (
            <>
              <div><dt className="text-faint">Available RAM</dt><dd className="font-medium text-foreground">{formatBytes(trace.event.memory.availableRamBytes)}</dd></div>
              <div><dt className="text-faint">Available VRAM</dt><dd className="font-medium text-foreground">{formatBytes(trace.event.memory.availableVramBytes)}</dd></div>
            </>
          )}
        </dl>
      ) : (
        <dl className="mt-2 grid gap-2 sm:grid-cols-2 lg:grid-cols-4">
          <div><dt className="text-faint">Duration</dt><dd className="font-medium text-foreground">{formatDuration(trace.event.timing.durationMs)}</dd></div>
          <div><dt className="text-faint">Temperature</dt><dd className="font-medium text-foreground">{trace.event.sampler.temperature ?? "—"}</dd></div>
          <div><dt className="text-faint">Max output tokens</dt><dd className="font-medium text-foreground">{trace.event.sampler.maxOutputTokens ?? "—"}</dd></div>
          <div><dt className="text-faint">Top P</dt><dd className="font-medium text-foreground">{trace.event.sampler.topP ?? "—"}</dd></div>
          <div><dt className="text-faint">Input tokens</dt><dd className="font-medium text-foreground">{trace.event.tokens.inputTokens ?? "—"}</dd></div>
          <div><dt className="text-faint">Output tokens</dt><dd className="font-medium text-foreground">{trace.event.tokens.outputTokens ?? "—"}</dd></div>
          <div><dt className="text-faint">Tokens/sec</dt><dd className="font-medium text-foreground">{trace.event.tokens.tokensPerSecond ? trace.event.tokens.tokensPerSecond.toFixed(1) : "—"}</dd></div>
        </dl>
      )}

      {trace.errorMessage && (
        <p className="mt-2 rounded-md border border-danger/30 bg-danger-soft px-2 py-1.5 text-danger">{trace.errorMessage}</p>
      )}

      {trace.unavailable.length > 0 && (
        <ul className="mt-2 space-y-0.5 text-faint">
          {trace.unavailable.map((note) => (
            <li key={note.field}>
              <span className="font-mono">{note.field}</span> unavailable: {note.reason}
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}

export function RuntimeHubTelemetry() {
  const runtimes = useRuntimeHubStore((state) => state.runtimes);
  const traces = useRuntimeHubStore((state) => state.traces);
  const supportBundle = useRuntimeHubStore((state) => state.supportBundle);
  const refreshTraces = useRuntimeHubStore((state) => state.refreshTraces);
  const exportSupportBundle = useRuntimeHubStore((state) => state.exportSupportBundle);
  const clearSupportBundle = useRuntimeHubStore((state) => state.clearSupportBundle);
  const busy = useRuntimeHubStore((state) => state.busy);
  const tracesError = useRuntimeHubStore((state) => state.errors["telemetry-traces"]);
  const bundleError = useRuntimeHubStore((state) => state.errors["telemetry-support-bundle"]);

  const [runtimeFilter, setRuntimeFilter] = useState("");
  const [exportStatus, setExportStatus] = useState<{ tone: "success" | "danger"; message: string } | null>(null);
  const [exporting, setExporting] = useState(false);

  useEffect(() => {
    void refreshTraces(runtimeFilter || null).catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [runtimeFilter]);

  const grouped = useMemo(() => groupTracesByModel(traces), [traces]);

  async function refresh() {
    await refreshTraces(runtimeFilter || null).catch(() => {});
  }

  async function preview() {
    setExportStatus(null);
    await exportSupportBundle().catch(() => {});
  }

  async function exportToFile() {
    if (!supportBundle) return;
    setExporting(true);
    setExportStatus(null);
    try {
      const destination = await save({
        defaultPath: supportBundleFileName(supportBundle),
        filters: [{ name: "Support bundle", extensions: ["json"] }],
      });
      if (!destination) return;
      await writeTextFile(destination, serializeSupportBundle(supportBundle));
      setExportStatus({ tone: "success", message: "Support bundle exported." });
    } catch (error) {
      setExportStatus({ tone: "danger", message: errorMessage(error) });
    } finally {
      setExporting(false);
    }
  }

  return (
    <div role="tabpanel" id="runtime-hub-panel-telemetry" aria-labelledby="runtime-hub-tab-telemetry" className="flex flex-col gap-5">
      <SectionHeading
        title="Runtime telemetry"
        description="Recent load and request traces per runtime — timing, memory/VRAM headroom, offload placement, and the sampler stats actually used. Prompt/response text and secrets are never captured; see the support bundle below for what is redacted."
      />

      <div className="flex flex-wrap items-end justify-between gap-3">
        <label className="text-xs font-medium text-muted">
          <span className="mb-1.5 block">Runtime</span>
          <select value={runtimeFilter} onChange={(event) => setRuntimeFilter(event.target.value)} className={CONTROL_CLASS}>
            <option value="">All runtimes</option>
            {runtimes.map((runtime) => (
              <option key={runtime.descriptor.runtimeId} value={runtime.descriptor.runtimeId}>
                {runtime.descriptor.label}
              </option>
            ))}
          </select>
        </label>
        <BusyButton type="button" busy={busy["telemetry-traces"]} onClick={() => void refresh()}>
          <RefreshCw size={14} aria-hidden="true" /> Refresh traces
        </BusyButton>
      </div>

      <ErrorNotice message={tracesError} />

      {traces.length === 0 ? (
        <p className="rounded-lg border border-dashed border-border p-4 text-center text-xs text-faint">
          No traces recorded yet. Load a model or send a diagnostics API request to populate this view.
        </p>
      ) : (
        Array.from(grouped.entries()).map(([modelId, modelTraces]) => (
          <section key={modelId} className="rounded-xl border border-border bg-surface p-4">
            <h4 className="text-sm font-semibold text-foreground">{modelId}</h4>
            <ul className="mt-3 space-y-2">
              {modelTraces.map((trace) => (
                <TraceCard key={trace.traceId} trace={trace} />
              ))}
            </ul>
          </section>
        ))
      )}

      <section className="rounded-xl border border-border bg-surface p-4" aria-labelledby="support-bundle-title">
        <SectionHeading
          title="Support bundle"
          description="Bundles recent traces, a bounded runtime log tail, and hardware/compatibility context into one exportable file. Redaction runs before this preview is ever shown, so what you see here is exactly what would be written to disk."
        />
        <div className="mt-3 flex flex-wrap gap-2">
          <BusyButton type="button" busy={busy["telemetry-support-bundle"]} onClick={() => void preview()}>
            <Eye size={14} aria-hidden="true" /> Preview support bundle
          </BusyButton>
          <BusyButton
            type="button"
            variant="primary"
            busy={exporting}
            disabled={!supportBundle}
            onClick={() => void exportToFile()}
          >
            <Download size={14} aria-hidden="true" /> Export to file
          </BusyButton>
          {supportBundle && (
            <BusyButton type="button" variant="ghost" onClick={() => clearSupportBundle()}>
              Clear preview
            </BusyButton>
          )}
        </div>

        <ErrorNotice message={bundleError} />
        {exportStatus?.tone === "success" && <SuccessNotice>{exportStatus.message}</SuccessNotice>}
        {exportStatus?.tone === "danger" && <ErrorNotice message={exportStatus.message} />}

        {supportBundle && (
          <div className="mt-3 space-y-3">
            <div className="flex flex-wrap items-center gap-2 rounded-lg border border-success/30 bg-success-soft px-3 py-2 text-xs text-success">
              <ShieldCheck size={14} aria-hidden="true" />
              <span>
                {supportBundle.redactionTotals.findingsRedacted} finding(s) redacted across {supportBundle.traces.length} trace(s)
                and {supportBundle.runtimeLogs.length} runtime log tail(s).
              </span>
            </div>
            <div>
              <p className="mb-1.5 text-xs font-medium text-muted">This bundle deliberately excludes</p>
              <ul className="list-disc space-y-0.5 pl-4 text-xs leading-5 text-muted">
                {supportBundle.excluded.map((item) => (
                  <li key={item}>{item}</li>
                ))}
              </ul>
            </div>
            <JsonView value={supportBundle} label="Full bundle contents (exactly what would be exported)" />
          </div>
        )}
      </section>
    </div>
  );
}

export default RuntimeHubTelemetry;
