import { useEffect, useState, type FormEvent } from "react";
import { FlaskConical, Gauge, PlayCircle, ShieldAlert } from "lucide-react";
import { StatusPill, type PillTone } from "../../ui";
import type { ConversionReport, LicenseRisk, QuantTypeDescriptor } from "../../../lib/runtimeHubClient";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import {
  BusyButton,
  CONTROL_CLASS,
  ErrorNotice,
  Field,
  formatBytes,
  formatDate,
  JsonView,
  labelize,
  SectionHeading,
} from "./RuntimeHubShared";

/** Maps a heuristic license risk to the `StatusPill` tone that best conveys
 * it: permissive is safe, restricted is the most serious, copyleft sits in
 * between, and unknown/unrecognized licenses are flagged neutrally for
 * manual review rather than alarmed over. */
export function licenseRiskTone(risk: LicenseRisk): PillTone {
  switch (risk) {
    case "permissive":
      return "success";
    case "restricted":
      return "danger";
    case "copyleft":
      return "warning";
    default:
      return "neutral";
  }
}

/** Picks a sensible default quantization choice from the backend's
 * descriptor list: `Q4_K_M` (the most common size/quality balance, see
 * `quantization.rs`'s tradeoff table) when it's offered, otherwise the first
 * entry, otherwise an empty string when the list hasn't loaded yet. */
export function pickDefaultQuantType(quantTypes: QuantTypeDescriptor[]): string {
  const preferred = quantTypes.find((entry) => entry.id === "Q4_K_M");
  return (preferred ?? quantTypes[0])?.id ?? "";
}

function BackendStatus() {
  const backends = useRuntimeHubStore((state) => state.quantizationBackends);
  if (!backends.length) {
    return <p className="text-sm text-muted">No quantization backends were reported.</p>;
  }
  return (
    <div className="flex flex-wrap gap-2" aria-label="Quantization backends">
      {backends.map((backend) => (
        <StatusPill key={backend.id} tone={backend.available ? "success" : "neutral"}>
          {backend.id} · {backend.available ? "Available" : "Not found"}
        </StatusPill>
      ))}
    </div>
  );
}

function ReportCard({ report }: { report: ConversionReport }) {
  return (
    <article className="rounded-lg border border-border bg-background p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h4 className="break-all font-mono text-sm font-semibold text-foreground">{report.quantChoice}</h4>
          <p className="mt-1 break-all font-mono text-xs text-muted">{report.source.path}</p>
        </div>
        <div className="flex flex-wrap gap-1.5">
          <StatusPill tone={report.tool.real ? "success" : "warning"}>
            {report.tool.name}
            {report.tool.version ? ` ${report.tool.version}` : ""} · {report.tool.real ? "Real" : "Passthrough"}
          </StatusPill>
          <StatusPill tone={report.eval.passed ? "success" : "danger"}>
            {report.eval.passed ? "Eval passed" : "Eval failed"}
          </StatusPill>
        </div>
      </div>

      <div className="mt-3 grid gap-2 text-xs text-muted sm:grid-cols-2">
        <span>Source {labelize(report.source.format)} · {formatBytes(report.source.sizeBytes)}</span>
        <span>Output {formatBytes(report.output.sizeBytes)}</span>
        <span className="break-all font-mono">Source sha256 {report.source.sha256.slice(0, 16)}…</span>
        <span className="break-all font-mono">Output sha256 {report.output.sha256.slice(0, 16)}…</span>
        <span>Generated {formatDate(report.generatedAtMs)}</span>
        <span>{report.allowRequantize ? "Requantize allowed" : "Requantize not allowed"}</span>
      </div>

      <p className="mt-3 text-xs leading-5 text-muted">{report.tradeoffNote}</p>
      <p className="mt-2 text-xs leading-5 text-muted">{report.eval.detail}</p>

      <div className="mt-3 rounded-md border border-border bg-surface-2 p-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <span className="text-xs font-medium text-foreground">
            {report.license.declaredName ?? "No declared license"}
          </span>
          <StatusPill tone={licenseRiskTone(report.license.risk)}>{labelize(report.license.risk)}</StatusPill>
        </div>
        {report.license.warning && (
          <p className="mt-2 flex items-start gap-1.5 text-xs leading-5 text-muted">
            <ShieldAlert size={13} className="mt-0.5 shrink-0" aria-hidden="true" /> {report.license.warning}
          </p>
        )}
      </div>

      <div className="mt-3">
        <JsonView value={report} label="Full reproducible report" />
      </div>
    </article>
  );
}

function ConvertForm() {
  const installedModels = useRuntimeHubStore((state) => state.installedModels);
  const quantTypes = useRuntimeHubStore((state) => state.quantizationQuantTypes);
  const convertPath = useRuntimeHubStore((state) => state.convertPathQuantization);
  const convertInstalledModel = useRuntimeHubStore((state) => state.convertInstalledModelQuantization);
  const busy = useRuntimeHubStore((state) => state.busy["quantization-convert"]);
  const error = useRuntimeHubStore((state) => state.errors["quantization-convert"]);

  const [mode, setMode] = useState<"installed" | "path">("installed");
  const [assetId, setAssetId] = useState("");
  const [sourcePath, setSourcePath] = useState("");
  const [quantChoice, setQuantChoice] = useState("");
  const [allowRequantize, setAllowRequantize] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);

  useEffect(() => {
    if (!quantChoice && quantTypes.length) setQuantChoice(pickDefaultQuantType(quantTypes));
  }, [quantTypes, quantChoice]);
  useEffect(() => {
    if (!assetId && installedModels.length) setAssetId(installedModels[0].assetId);
  }, [installedModels, assetId]);

  const selectedNote = quantTypes.find((entry) => entry.id === quantChoice)?.note;

  async function submit(event: FormEvent) {
    event.preventDefault();
    setLocalError(null);
    if (!quantChoice) {
      setLocalError("Choose a quantization type first.");
      return;
    }
    if (mode === "installed") {
      if (!assetId) {
        setLocalError("Choose an installed model to convert.");
        return;
      }
      await convertInstalledModel(assetId, null, quantChoice, allowRequantize).catch(() => {});
    } else {
      if (!sourcePath.trim()) {
        setLocalError("Enter an absolute path to a GGUF file or safetensors checkout.");
        return;
      }
      await convertPath(sourcePath.trim(), quantChoice, allowRequantize).catch(() => {});
    }
  }

  return (
    <form onSubmit={submit} className="flex flex-col gap-4 rounded-lg border border-border bg-background p-4">
      <div className="grid gap-4 sm:grid-cols-2">
        <Field label="Source">
          <select value={mode} onChange={(event) => setMode(event.target.value as "installed" | "path")} className={CONTROL_CLASS}>
            <option value="installed">Installed Runtime Hub model (active version)</option>
            <option value="path">File path on disk</option>
          </select>
        </Field>
        {mode === "installed" ? (
          <Field label="Installed model" hint="Reuses this model's verified catalog license.">
            <select value={assetId} onChange={(event) => setAssetId(event.target.value)} className={CONTROL_CLASS} disabled={!installedModels.length}>
              {!installedModels.length && <option value="">No installed models</option>}
              {installedModels.map((model) => (
                <option key={model.assetId} value={model.assetId}>
                  {model.displayName}
                </option>
              ))}
            </select>
          </Field>
        ) : (
          <Field label="Source path" hint="A .gguf file, or a directory containing .safetensors shards.">
            <input value={sourcePath} onChange={(event) => setSourcePath(event.target.value)} className={`${CONTROL_CLASS} font-mono`} placeholder="/path/to/model.gguf" autoComplete="off" />
          </Field>
        )}
        <Field label="Quantization type" hint={selectedNote}>
          <select value={quantChoice} onChange={(event) => setQuantChoice(event.target.value)} className={CONTROL_CLASS} disabled={!quantTypes.length}>
            {!quantTypes.length && <option value="">Loading…</option>}
            {quantTypes.map((entry) => (
              <option key={entry.id} value={entry.id}>
                {entry.cliName}
              </option>
            ))}
          </select>
        </Field>
        <label className="flex min-h-11 cursor-pointer items-center gap-2 self-end text-xs text-foreground">
          <input
            type="checkbox"
            checked={allowRequantize}
            onChange={(event) => setAllowRequantize(event.target.checked)}
            className="h-4 w-4 rounded border-border accent-[var(--color-accent)] focus-visible:ring-2 focus-visible:ring-accent"
          />
          Allow requantizing an already-quantized source (further reduces quality)
        </label>
      </div>

      <ErrorNotice message={localError ?? error} />

      <div className="flex justify-end">
        <BusyButton type="submit" variant="primary" busy={busy} disabled={!quantChoice}>
          <PlayCircle size={15} aria-hidden="true" /> Convert &amp; quantize
        </BusyButton>
      </div>
    </form>
  );
}

export function RuntimeHubQuantization() {
  const loaded = useRuntimeHubStore((state) => state.quantizationQuantTypes.length > 0);
  const refreshQuantization = useRuntimeHubStore((state) => state.refreshQuantization);
  const refreshing = useRuntimeHubStore((state) => state.busy["quantization-refresh"]);
  const refreshError = useRuntimeHubStore((state) => state.errors["quantization-refresh"]);
  const reports = useRuntimeHubStore((state) => state.quantizationReports);

  useEffect(() => {
    if (!loaded) void refreshQuantization().catch(() => {});
  }, [loaded, refreshQuantization]);

  return (
    <div role="tabpanel" id="runtime-hub-panel-quantization" aria-labelledby="runtime-hub-tab-quantization" className="flex flex-col gap-5">
      <SectionHeading
        title="Model conversion and quantization workbench"
        description="Quantize an installed model or an arbitrary GGUF/safetensors path. Every conversion produces a reproducible report: source/output digests, the exact tool used, a license risk check, and a real structural GGUF-parses smoke test."
      />

      <section className="flex flex-col gap-2" aria-labelledby="quantization-backends-heading">
        <div className="flex items-center gap-2">
          <Gauge size={15} className="text-muted" aria-hidden="true" />
          <h3 id="quantization-backends-heading" className="text-sm font-semibold text-foreground">Available backends</h3>
        </div>
        <ErrorNotice message={refreshError} />
        <BackendStatus />
        {refreshing && <p className="text-xs text-muted">Refreshing…</p>}
      </section>

      <ConvertForm />

      <section className="flex flex-col gap-3" aria-labelledby="quantization-reports-heading">
        <div className="flex items-center gap-2">
          <FlaskConical size={15} className="text-muted" aria-hidden="true" />
          <h3 id="quantization-reports-heading" className="text-sm font-semibold text-foreground">Conversion reports (this session)</h3>
        </div>
        {reports.length ? (
          <div className="flex flex-col gap-3" aria-live="polite">
            {reports.map((report) => (
              <ReportCard key={report.conversionId} report={report} />
            ))}
          </div>
        ) : (
          <div className="rounded-lg border border-dashed border-border p-6 text-center text-sm text-muted">
            No conversions have been run yet in this session.
          </div>
        )}
      </section>
    </div>
  );
}

export default RuntimeHubQuantization;
