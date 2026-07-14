import { Boxes, Cpu, Database, Gauge, HardDrive, Server } from "lucide-react";
import { StatusPill } from "../../ui";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import { ErrorNotice, formatBytes, labelize, SectionHeading } from "./RuntimeHubShared";

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
