import { useEffect, useState } from "react";
import { Boxes, Cpu, Database, Gauge, HardDrive, Server } from "lucide-react";
import { StatusPill } from "../../ui";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import { resolveEdgeRuntimeProfile } from "../../../lib/runtimeEdgeProfiles";
import type { BenchmarkHistoryEntry, M3AcceleratorStatus } from "../../../lib/runtimeHubClient";
import { runtimeHubClient } from "../../../lib/runtimeHubClient";
import { ErrorNotice, formatBytes, labelize, SectionHeading } from "./RuntimeHubShared";

const COMPATIBILITY_TONE: Record<M3AcceleratorStatus, "success" | "warning" | "danger" | "neutral"> = {
  available: "success",
  not_detected: "neutral",
  driver_too_old: "danger",
  tool_missing: "warning",
  unsupported: "neutral",
};

const COMPATIBILITY_LABEL: Record<M3AcceleratorStatus, string> = {
  available: "Available",
  not_detected: "Not detected",
  driver_too_old: "Driver too old",
  tool_missing: "Tool missing",
  unsupported: "Unsupported here",
};

function MetricCard({
  icon: Icon,
  label,
  value,
  detail,
}: {
  icon: typeof Cpu;
  label: string;
  value: string;
  detail: string;
}) {
  return (
    <div className="rounded-lg border border-border bg-background p-4">
      <div className="flex items-center gap-2 text-muted">
        <Icon size={16} aria-hidden="true" />
        <p className="text-xs font-medium uppercase tracking-wide">{label}</p>
      </div>
      <p className="mt-3 text-lg font-semibold text-foreground">{value}</p>
      <p className="mt-1 text-xs text-muted">{detail}</p>
    </div>
  );
}

export function RuntimeHubOverview() {
  const hardware = useRuntimeHubStore((state) => state.hardware);
  const profile = useRuntimeHubStore((state) => state.profile);
  const storage = useRuntimeHubStore((state) => state.storage);
  const installedModels = useRuntimeHubStore((state) => state.installedModels);
  const runtimes = useRuntimeHubStore((state) => state.runtimes);
  const error = useRuntimeHubStore((state) => state.errors.overview);
  const compatibilityReport = useRuntimeHubStore((state) => state.compatibilityReport);
  const compatibilityError = useRuntimeHubStore((state) => state.errors.compatibility);

  // The edge profile's throughput line defers to "the local benchmark" — this is
  // what lets it stop deferring once one has actually been run. A read failure
  // leaves the list empty, which is the same state as "never benchmarked" and
  // renders the same hedge, so there is nothing to report.
  const [benchmarks, setBenchmarks] = useState<BenchmarkHistoryEntry[]>([]);
  useEffect(() => {
    let cancelled = false;
    runtimeHubClient
      .benchmarkHistory()
      .then((entries) => {
        if (!cancelled) setBenchmarks(entries);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  const usedPercent = storage?.quotaBytes
    ? Math.min(100, Math.max(0, (storage.usedBytes / storage.quotaBytes) * 100))
    : 0;
  const accelerators = hardware?.platform.accelerators.filter((accelerator) => accelerator.available) ?? [];
  const matrix = [
    {
      label: "Ollama",
      platform: hardware?.platform.supported_runtimes.includes("ollama") ?? false,
      driver: runtimes.some((runtime) => runtime.descriptor.kind === "ollama"),
      backend: "OpenAI-compatible local adapter",
    },
    {
      label: "Managed llama.cpp",
      platform: hardware?.platform.supported_runtimes.includes("llama_cpp") ?? false,
      driver: runtimes.some((runtime) => runtime.descriptor.kind === "llama_cpp"),
      backend: "App-owned process and loopback port",
    },
    {
      label: "MLX",
      platform: Boolean(hardware && hardware.platform.os === "macos" && hardware.platform.arch === "aarch64" && hardware.platform.accelerators.some((accelerator) => accelerator.kind === "metal" && accelerator.available)),
      driver: runtimes.some((runtime) => runtime.descriptor.kind === "mlx"),
      backend: "Verified app-private Apple Silicon service",
    },
  ];
  const edgeProfile = hardware && profile
    ? resolveEdgeRuntimeProfile(hardware, profile, compatibilityReport, benchmarks)
    : null;

  return (
    <div role="tabpanel" id="runtime-hub-panel-overview" aria-labelledby="runtime-hub-tab-overview" className="flex flex-col gap-5">
      <ErrorNotice message={error} />

      <SectionHeading
        title="System fit"
        description="Live hardware data is used to grade catalog models before you download them."
      />
      <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
        <MetricCard
          icon={Cpu}
          label="Compute profile"
          value={profile ? labelize(profile.tier) : "Loading…"}
          detail={
            profile
              ? `${profile.recommended_process_slots} process slot${profile.recommended_process_slots === 1 ? "" : "s"} · ${labelize(profile.preferred_accelerator)}`
              : "Detecting CPU and accelerators"
          }
        />
        <MetricCard
          icon={Gauge}
          label="Available memory"
          value={formatBytes(hardware?.available_ram_bytes)}
          detail={`${formatBytes(hardware?.total_ram_bytes)} total system RAM`}
        />
        <MetricCard
          icon={Server}
          label="Runtime drivers"
          value={String(runtimes.length)}
          detail={`${runtimes.filter((runtime) => runtime.canInfer).length} ready for inference`}
        />
        <MetricCard
          icon={Boxes}
          label="Installed models"
          value={String(installedModels.length)}
          detail={`${installedModels.reduce((sum, model) => sum + model.versions.length, 0)} verified version${installedModels.reduce((sum, model) => sum + model.versions.length, 0) === 1 ? "" : "s"}`}
        />
        <MetricCard
          icon={HardDrive}
          label="Model storage"
          value={formatBytes(storage?.usedBytes)}
          detail={`${formatBytes(storage?.availableForModelsBytes)} available for models`}
        />
        <MetricCard
          icon={Database}
          label="Storage reserve"
          value={formatBytes(storage?.reserveBytes)}
          detail={storage?.pendingDownloadBytes ? `${formatBytes(storage.pendingDownloadBytes)} downloading` : "No active downloads"}
        />
      </div>

      {storage && (
        <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="runtime-storage-heading">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div>
              <h3 id="runtime-storage-heading" className="text-sm font-semibold text-foreground">Storage quota</h3>
              <p className="mt-1 break-all font-mono text-xs text-muted">{storage.root}</p>
            </div>
            <StatusPill tone={usedPercent >= 90 ? "danger" : usedPercent >= 75 ? "warning" : "success"}>
              {usedPercent.toFixed(1)}% used
            </StatusPill>
          </div>
          <div
            role="progressbar"
            aria-label="Model storage used"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={Math.round(usedPercent)}
            className="mt-4 h-2 overflow-hidden rounded-full bg-surface-2"
          >
            <div className="h-full rounded-full bg-accent transition-[width] motion-reduce:transition-none" style={{ width: `${usedPercent}%` }} />
          </div>
          <div className="mt-2 flex flex-wrap justify-between gap-2 text-xs text-muted">
            <span>{formatBytes(storage.usedBytes)} used</span>
            <span>{formatBytes(storage.quotaBytes)} quota</span>
          </div>
        </section>
      )}

      <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="runtime-accelerators-heading">
        <SectionHeading
          title="Detected acceleration"
          description={`${hardware?.platform.os ?? "Unknown OS"} · ${hardware?.platform.arch ?? "unknown architecture"} · ${hardware?.logical_cpu_count ?? 0} logical CPUs`}
        />
        <div id="runtime-accelerators-heading" className="mt-3 flex flex-wrap gap-2">
          {accelerators.length ? (
            accelerators.map((accelerator) => (
              <StatusPill key={accelerator.kind} tone="success">
                {labelize(accelerator.kind)}
                {accelerator.device_names.length ? ` · ${accelerator.device_names.join(", ")}` : ""}
                {accelerator.total_memory_bytes != null ? ` · ${formatBytes(accelerator.total_memory_bytes)} total` : ""}
                {accelerator.available_memory_bytes != null ? ` · ${formatBytes(accelerator.available_memory_bytes)} free` : ""}
              </StatusPill>
            ))
          ) : (
            <p className="text-sm text-muted">No supported accelerator was reported; CPU inference remains available where supported.</p>
          )}
        </div>
        {hardware && <p className="mt-3 text-xs text-muted">Snapshot captured {new Date(hardware.captured_at_ms).toLocaleString()}.</p>}
      </section>

      <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="runtime-edge-profile-heading">
        <SectionHeading
          title="Edge device runtime profile"
          description="A conservative local recommendation derived from detected hardware. Runtime compatibility and a measured benchmark still gate actual use."
        />
        {edgeProfile ? (
          <div id="runtime-edge-profile-heading" className="mt-3 grid gap-3 lg:grid-cols-[minmax(0,1.2fr)_minmax(0,1fr)]">
            <div className="rounded-md border border-border bg-surface-2 p-3">
              <div className="flex flex-wrap items-center gap-2">
                <p className="text-sm font-semibold text-foreground">{edgeProfile.label}</p>
                <StatusPill tone={edgeProfile.confidence === "confirmed" ? "success" : "neutral"}>
                  {edgeProfile.confidence}
                </StatusPill>
              </div>
              <p className="mt-2 text-xs leading-5 text-muted">{edgeProfile.summary}</p>
              <dl className="mt-3 grid gap-2 text-xs sm:grid-cols-2">
                <div><dt className="text-faint">Runtime</dt><dd className="font-medium text-foreground">{labelize(edgeProfile.recommendedRuntime)}</dd></div>
                <div><dt className="text-faint">Model class</dt><dd className="font-medium text-foreground">{edgeProfile.recommendedModelClass}</dd></div>
                <div><dt className="text-faint">Safe context start</dt><dd className="font-medium text-foreground">{edgeProfile.contextTokens.toLocaleString()} tokens</dd></div>
                <div><dt className="text-faint">Process slots</dt><dd className="font-medium text-foreground">{edgeProfile.processSlots}</dd></div>
              </dl>
              <p className="mt-3 text-xs leading-5 text-muted">{edgeProfile.expectedSpeed}</p>
            </div>
            <div className="rounded-md border border-border bg-surface-2 p-3 text-xs leading-5 text-muted">
              <p className="font-semibold text-foreground">Required components</p>
              <ul className="mt-1 list-disc pl-5">
                {edgeProfile.requiredComponents.map((entry) => <li key={entry}>{entry}</li>)}
              </ul>
              <p className="mt-3 font-semibold text-foreground">Safe fallbacks</p>
              <ul className="mt-1 list-disc pl-5">
                {edgeProfile.fallbacks.map((entry) => <li key={entry}>{entry}</li>)}
              </ul>
              <p className="mt-3 font-semibold text-foreground">Evidence</p>
              <ul className="mt-1 list-disc pl-5">
                {edgeProfile.evidence.map((entry) => <li key={entry}>{entry}</li>)}
              </ul>
            </div>
          </div>
        ) : (
          <p id="runtime-edge-profile-heading" className="mt-3 text-sm text-muted">Loading the local hardware profile…</p>
        )}
      </section>

      <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="runtime-compatibility-heading">
        <SectionHeading
          title="Hardware compatibility matrix (Driver Doctor)"
          description="What will work, what falls back to CPU, and what needs a driver or runtime update — checked before you download, load, or install."
        />
        <ErrorNotice message={compatibilityError} />
        <div id="runtime-compatibility-heading" className="mt-3 overflow-x-auto">
          <table className="w-full min-w-[40rem] text-left text-xs">
            <thead className="text-muted">
              <tr className="border-b border-border">
                <th className="px-2 py-2 font-medium">Backend</th>
                <th className="px-2 py-2 font-medium">Status</th>
                <th className="px-2 py-2 font-medium">Runs work here</th>
                <th className="px-2 py-2 font-medium">Driver / compute</th>
                <th className="px-2 py-2 font-medium">Details</th>
              </tr>
            </thead>
            <tbody>
              {compatibilityReport ? (
                compatibilityReport.accelerators.map((accelerator) => (
                  <tr key={accelerator.kind} className="border-b border-border/60 last:border-0">
                    <td className="px-2 py-3 font-medium text-foreground">{labelize(accelerator.kind)}</td>
                    <td className="px-2 py-3">
                      <StatusPill tone={COMPATIBILITY_TONE[accelerator.status]}>
                        {COMPATIBILITY_LABEL[accelerator.status]}
                        {!accelerator.confirmed ? " (unconfirmed)" : ""}
                      </StatusPill>
                    </td>
                    {/* Deliberately its own column rather than folded into the
                        status pill: "detected" and "this app can use it" are
                        different facts, and three of the six backends are the
                        first without being the second. */}
                    <td className="px-2 py-3">
                      <StatusPill tone={accelerator.execution.state === "executes" ? "success" : "neutral"}>
                        {accelerator.execution.state === "executes" ? "Yes" : "Detection only"}
                      </StatusPill>
                    </td>
                    <td className="px-2 py-3 text-muted">
                      {[accelerator.driverVersion, accelerator.computeCapability].filter(Boolean).join(" · ") || "—"}
                    </td>
                    <td className="px-2 py-3 text-muted">
                      {accelerator.summary}
                      {/* The reason, next to the claim it qualifies. A backend
                          nothing runs on is the case a user most needs an
                          explanation for, and it is the one the summary — which
                          describes the *hardware* — cannot give. */}
                      <span className="mt-1 block text-faint">
                        {accelerator.execution.state === "executes"
                          ? `Runs on ${accelerator.execution.via}.`
                          : accelerator.execution.reason}
                      </span>
                    </td>
                  </tr>
                ))
              ) : (
                <tr>
                  <td colSpan={5} className="px-2 py-3 text-muted">Detecting hardware compatibility…</td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
        {compatibilityReport?.jetson.detected && (
          <p className="mt-3 text-xs text-muted">
            Jetson device detected{compatibilityReport.jetson.model ? `: ${compatibilityReport.jetson.model}` : ""}. Use Jetson-appropriate CUDA/TensorRT builds rather than desktop CUDA packages.
          </p>
        )}
        {compatibilityReport?.hybridGraphicsDetected && (
          <p className="mt-2 text-xs text-muted">Hybrid or multi-GPU configuration detected; device selection may need to be explicit rather than automatic.</p>
        )}
        {compatibilityReport?.notes.map((note, index) => (
          <p key={`compat-note-${index}`} className="mt-2 text-xs text-muted">{note}</p>
        ))}
      </section>

      <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="runtime-capability-matrix-heading">
        <SectionHeading
          title="Platform and backend matrix"
          description="Platform eligibility and live factory discovery are reported independently; a supported platform does not imply that a verified runtime package is installed."
        />
        <div id="runtime-capability-matrix-heading" className="mt-3 overflow-x-auto">
          <table className="w-full min-w-[34rem] text-left text-xs">
            <thead className="text-muted">
              <tr className="border-b border-border">
                <th className="px-2 py-2 font-medium">Runtime</th>
                <th className="px-2 py-2 font-medium">Platform</th>
                <th className="px-2 py-2 font-medium">Live driver</th>
                <th className="px-2 py-2 font-medium">Boundary</th>
              </tr>
            </thead>
            <tbody>
              {matrix.map((entry) => (
                <tr key={entry.label} className="border-b border-border/60 last:border-0">
                  <td className="px-2 py-3 font-medium text-foreground">{entry.label}</td>
                  <td className="px-2 py-3"><StatusPill tone={entry.platform ? "success" : "neutral"}>{entry.platform ? "Supported" : "Unavailable"}</StatusPill></td>
                  <td className="px-2 py-3"><StatusPill tone={entry.driver ? "success" : "warning"}>{entry.driver ? "Discovered" : "Not installed"}</StatusPill></td>
                  <td className="px-2 py-3 text-muted">{entry.backend}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>
    </div>
  );
}
