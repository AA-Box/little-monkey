import { useEffect, useMemo, useState } from "react";
import { Activity, FileText, Play, RefreshCw, Save, Square, Wrench } from "lucide-react";
import { Button, StatusPill } from "../../ui";
import type {
  AdvancedSettingCapability,
  KeepAlive,
  M3RuntimeCapability,
  M3RuntimeKind,
  RunningModel,
  SettingValue,
} from "../../../lib/runtimeHubClient";
import { useRuntimeHubStore, type RuntimeDetail } from "../../../store/runtimeHubStore";
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
  Toggle,
} from "./RuntimeHubShared";

function statusState(detail: RuntimeDetail | undefined): string {
  if (!detail?.status) return "unknown";
  if (detail.status.runtimeType === "adapter") return detail.status.status.state;
  return detail.status.status.state;
}

function statusTone(state: string): "neutral" | "success" | "warning" | "danger" {
  if (state === "ready" || state === "running") return "success";
  if (state === "starting" || state === "degraded") return "warning";
  if (state === "error" || state === "unreachable" || state === "unavailable") return "danger";
  return "neutral";
}

function runningModels(detail: RuntimeDetail | undefined): RunningModel[] {
  if (!detail?.status) return [];
  if (detail.status.runtimeType === "adapter") return detail.status.running_models;
  if (detail.status.status.state !== "running") return [];
  const handle = detail.status.status.handle as { modelId?: string };
  const metrics = detail.status.status.metrics;
  return [{
    runtime_id: "mlx",
    model_id: handle.modelId ?? "unknown",
    size_bytes: 0,
    memory_bytes: metrics.residentMemoryBytes,
    vram_bytes: metrics.unifiedMemoryBytes,
    digest: null,
    expires_at: null,
    ownership: "app_managed",
  }];
}

/** Runtime-specific load policy kept pure so the UI cannot accidentally send
 * an Ollama-only keep-alive value to managed llama.cpp. */
export function keepAliveForRuntime(
  kind: M3RuntimeKind,
  mode: "duration" | "forever",
  minutes: number,
): KeepAlive | null {
  if (kind === "llama_cpp") return null;
  if (mode === "forever") return { mode: "forever" };
  const boundedMinutes = Number.isFinite(minutes)
    ? Math.min(1_440, Math.max(1, Math.round(minutes)))
    : 10;
  return { mode: "duration_ms", milliseconds: boundedMinutes * 60_000 };
}

function SettingControl({
  capability,
  value,
  onChange,
}: {
  capability: AdvancedSettingCapability;
  value: SettingValue;
  onChange: (value: SettingValue) => void;
}) {
  const schema = capability.schema;
  if (schema.type === "boolean" && value.type === "boolean") {
    return (
      <Toggle
        checked={value.value}
        onChange={(next) => onChange({ type: "boolean", value: next })}
        label={capability.label}
        description={`${capability.description}${capability.restart_required ? " Restart required." : ""}`}
      />
    );
  }
  if (schema.type === "choice" && value.type === "choice") {
    return (
      <Field label={capability.label} hint={`${capability.description}${capability.restart_required ? " Restart required." : ""}`}>
        <select value={value.value} onChange={(event) => onChange({ type: "choice", value: event.target.value })} className={CONTROL_CLASS}>
          {schema.options.map((option) => <option key={option} value={option}>{option}</option>)}
        </select>
      </Field>
    );
  }
  if (schema.type === "text" && value.type === "text") {
    return (
      <Field label={capability.label} hint={`${capability.description} Up to ${schema.max_bytes} bytes.${capability.restart_required ? " Restart required." : ""}`}>
        <input value={value.value} onChange={(event) => onChange({ type: "text", value: event.target.value })} className={CONTROL_CLASS} />
      </Field>
    );
  }
  if (schema.type === "integer" && value.type === "integer") {
    return (
      <Field label={capability.label} hint={`${capability.description}${capability.restart_required ? " Restart required." : ""}`}>
        <input
          type="number"
          min={schema.min}
          max={schema.max}
          step={schema.step}
          value={value.value}
          onChange={(event) => onChange({ type: "integer", value: Number(event.target.value) })}
          className={CONTROL_CLASS}
        />
      </Field>
    );
  }
  if (schema.type === "float" && value.type === "float") {
    return (
      <Field label={capability.label} hint={`${capability.description}${capability.restart_required ? " Restart required." : ""}`}>
        <input
          type="number"
          min={schema.min}
          max={schema.max}
          step={schema.step}
          value={value.value}
          onChange={(event) => onChange({ type: "float", value: Number(event.target.value) })}
          className={CONTROL_CLASS}
        />
      </Field>
    );
  }
  if (schema.type === "duration_ms" && value.type === "duration_ms") {
    return (
      <Field label={capability.label} hint={`${capability.description} Milliseconds.${capability.restart_required ? " Restart required." : ""}`}>
        <input
          type="number"
          min={schema.min}
          max={schema.max}
          step={schema.step}
          value={value.value}
          onChange={(event) => onChange({ type: "duration_ms", value: Number(event.target.value) })}
          className={CONTROL_CLASS}
        />
      </Field>
    );
  }
  return <p className="text-xs text-danger">Unsupported setting schema for {capability.key}</p>;
}

function RuntimeCard({ runtime }: { runtime: M3RuntimeCapability }) {
  const runtimeId = runtime.descriptor.runtimeId;
  const installedModels = useRuntimeHubStore((state) => state.installedModels);
  const detail = useRuntimeHubStore((state) => state.runtimeDetails[runtimeId]);
  const busy = useRuntimeHubStore((state) => state.busy);
  const errors = useRuntimeHubStore((state) => state.errors);
  const refreshRuntime = useRuntimeHubStore((state) => state.refreshRuntime);
  const loadModel = useRuntimeHubStore((state) => state.loadModel);
  const unloadModel = useRuntimeHubStore((state) => state.unloadModel);
  const saveConfig = useRuntimeHubStore((state) => state.setRuntimeConfig);

  const compatibleModels = installedModels.filter((model) => model.runtime === runtime.descriptor.kind);
  const [assetId, setAssetId] = useState(compatibleModels[0]?.assetId ?? "");
  const [replaceExisting, setReplaceExisting] = useState(false);
  const [keepAliveMode, setKeepAliveMode] = useState<"duration" | "forever">("duration");
  const [keepAliveMinutes, setKeepAliveMinutes] = useState(10);
  const [forceByModel, setForceByModel] = useState<Record<string, boolean>>({});
  const [showLogs, setShowLogs] = useState(false);
  const [showMetrics, setShowMetrics] = useState(false);
  const [showSettings, setShowSettings] = useState(false);
  const [settings, setSettings] = useState<Record<string, SettingValue>>(() =>
    Object.fromEntries(runtime.settings.map((setting) => [setting.key, setting.default_value])),
  );

  useEffect(() => {
    if (!assetId && compatibleModels[0]) setAssetId(compatibleModels[0].assetId);
  }, [assetId, compatibleModels]);

  useEffect(() => {
    if (detail?.config) setSettings((current) => ({ ...current, ...detail.config }));
  }, [detail?.config]);

  const state = statusState(detail);
  const resident = runningModels(detail);
  const inventory = detail?.inventory?.models ?? [];
  // Managed llama.cpp owns an explicit process whose lifetime is controlled
  // by Load/Unload. Its adapter intentionally rejects an Ollama-style
  // keep_alive value, so never manufacture one merely because the shared UI
  // happens to expose that control for runtimes which support it.
  const supportsKeepAlive = runtime.descriptor.kind !== "llama_cpp";

  function handleLoad() {
    if (!assetId) return;
    void loadModel({
      runtimeId,
      assetId,
      keepAlive: keepAliveForRuntime(runtime.descriptor.kind, keepAliveMode, keepAliveMinutes),
      replaceExisting,
    }).catch(() => {});
  }

  return (
    <article className="rounded-lg border border-border bg-background p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="text-sm font-semibold text-foreground">{runtime.descriptor.label}</h3>
            <StatusPill tone={statusTone(state)}>{labelize(state)}</StatusPill>
          </div>
          <p className="mt-1 font-mono text-xs text-muted">
            {runtimeId} · {labelize(runtime.descriptor.kind)} · {runtime.descriptor.managed ? "managed" : "external"}
          </p>
        </div>
        <BusyButton
          type="button"
          busy={busy[`runtime:${runtimeId}`]}
          onClick={() => void refreshRuntime(runtimeId).catch(() => {})}
        >
          <RefreshCw size={15} aria-hidden="true" /> Refresh
        </BusyButton>
      </div>
      <ErrorNotice message={errors[`runtime:${runtimeId}`]} />

      <div className="mt-4 grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <div className="rounded-md bg-surface-2 p-3">
          <p className="text-xs text-muted">Inference</p>
          <p className="mt-1 text-sm font-medium text-foreground">{runtime.canInfer ? "Supported" : "Unavailable"}</p>
        </div>
        <div className="rounded-md bg-surface-2 p-3">
          <p className="text-xs text-muted">Resident models</p>
          <p className="mt-1 text-sm font-medium text-foreground">{resident.length}</p>
        </div>
        <div className="rounded-md bg-surface-2 p-3">
          <p className="text-xs text-muted">Inventory</p>
          <p className="mt-1 text-sm font-medium text-foreground">{inventory.length} models</p>
        </div>
        <div className="rounded-md bg-surface-2 p-3">
          <p className="text-xs text-muted">Last checked</p>
          <p className="mt-1 text-sm font-medium text-foreground">{formatDate(detail?.refreshedAt)}</p>
        </div>
      </div>

      {runtime.canLoad && (
        <section className="mt-5 border-t border-border pt-4" aria-label={`Load a model in ${runtime.descriptor.label}`}>
          <SectionHeading title="Load model" description="Only verified models compatible with this runtime are listed." />
          <div className="mt-3 grid gap-3 sm:grid-cols-2">
            <Field label="Installed model">
              <select value={assetId} onChange={(event) => setAssetId(event.target.value)} className={CONTROL_CLASS} disabled={!compatibleModels.length}>
                {!compatibleModels.length && <option value="">No compatible models installed</option>}
                {compatibleModels.map((model) => <option key={model.assetId} value={model.assetId}>{model.displayName} · {model.variantId}</option>)}
              </select>
            </Field>
            {supportsKeepAlive && <Field label="Keep alive">
              <select value={keepAliveMode} onChange={(event) => setKeepAliveMode(event.target.value as "duration" | "forever")} className={CONTROL_CLASS}>
                <option value="duration">Timed</option>
                <option value="forever">Until manually unloaded</option>
              </select>
            </Field>}
            {supportsKeepAlive && keepAliveMode === "duration" && (
              <Field label="Keep-alive minutes">
                <input type="number" min={1} max={1440} value={keepAliveMinutes} onChange={(event) => setKeepAliveMinutes(Number(event.target.value))} className={CONTROL_CLASS} />
              </Field>
            )}
            <Toggle checked={replaceExisting} onChange={setReplaceExisting} label="Replace currently loaded model" description="Unload app-managed residents before loading this model." />
          </div>
          <ErrorNotice message={errors[`load:${runtimeId}`]} />
          <div className="mt-3 flex justify-end">
            <BusyButton type="button" variant="primary" busy={busy[`load:${runtimeId}`]} disabled={!assetId} onClick={handleLoad}>
              <Play size={15} aria-hidden="true" /> Load model
            </BusyButton>
          </div>
        </section>
      )}

      {resident.length > 0 && runtime.canUnload && (
        <section className="mt-5 border-t border-border pt-4" aria-label={`Resident models in ${runtime.descriptor.label}`}>
          <SectionHeading title="Resident models" description="Pre-existing or externally owned processes are preserved unless you explicitly override ownership." />
          <div className="mt-3 flex flex-col gap-2">
            {resident.map((model) => {
              const key = `unload:${runtimeId}:${model.model_id}`;
              return (
                <div key={model.model_id} className="rounded-md border border-border bg-surface-2 p-3">
                  <div className="flex flex-wrap items-center justify-between gap-3">
                    <div className="min-w-0">
                      <p className="break-all font-mono text-sm text-foreground">{model.model_id}</p>
                      <p className="mt-1 text-xs text-muted">{labelize(model.ownership)} · {formatBytes(model.memory_bytes)} RAM · {formatBytes(model.vram_bytes)} VRAM</p>
                    </div>
                    <BusyButton
                      type="button"
                      variant="danger"
                      busy={busy[key]}
                      onClick={() => void unloadModel({ runtimeId, modelId: model.model_id, forceExactOwner: forceByModel[model.model_id] ?? false }).catch(() => {})}
                    >
                      <Square size={14} aria-hidden="true" /> Unload
                    </BusyButton>
                  </div>
                  {model.ownership !== "app_managed" && (
                    <label className="mt-2 flex min-h-11 cursor-pointer items-center gap-2 text-xs text-warning">
                      <input
                        type="checkbox"
                        checked={forceByModel[model.model_id] ?? false}
                        onChange={(event) => setForceByModel((current) => ({ ...current, [model.model_id]: event.target.checked }))}
                        className="h-4 w-4 accent-[var(--color-accent)]"
                      />
                      Force exact-owner unload for this {labelize(model.ownership)} model
                    </label>
                  )}
                  <ErrorNotice message={errors[key]} />
                </div>
              );
            })}
          </div>
        </section>
      )}

      <div className="mt-5 flex flex-wrap gap-2 border-t border-border pt-4">
        {runtime.canLogs && (
          <Button type="button" className="min-h-11" aria-expanded={showLogs} onClick={() => setShowLogs((value) => !value)}>
            <FileText size={15} aria-hidden="true" /> {showLogs ? "Hide logs" : "Show logs"}
          </Button>
        )}
        {runtime.canMetrics && (
          <Button type="button" className="min-h-11" aria-expanded={showMetrics} onClick={() => setShowMetrics((value) => !value)}>
            <Activity size={15} aria-hidden="true" /> {showMetrics ? "Hide metrics" : "Show metrics"}
          </Button>
        )}
        {runtime.settings.length > 0 && (
          <Button type="button" className="min-h-11" aria-expanded={showSettings} onClick={() => setShowSettings((value) => !value)}>
            <Wrench size={15} aria-hidden="true" /> {showSettings ? "Hide advanced settings" : "Advanced settings"}
          </Button>
        )}
      </div>

      {showLogs && detail?.logs && (
        <div className="mt-4">
          <p className="mb-1.5 text-xs font-medium text-muted">Runtime log tail{detail.logs.truncated ? " · truncated" : ""}</p>
          <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all rounded-lg border border-border bg-surface-2 p-3 font-mono text-xs leading-5 text-foreground">{detail.logs.text || "No log output."}</pre>
        </div>
      )}
      {showMetrics && detail?.metrics && <div className="mt-4"><JsonView label="Live runtime metrics" value={detail.metrics} /></div>}

      {showSettings && runtime.settings.length > 0 && (
        <section className="mt-4 rounded-lg border border-border bg-surface p-4" aria-label={`Advanced settings for ${runtime.descriptor.label}`}>
          <div className="grid gap-4 sm:grid-cols-2">
            {runtime.settings.map((capability) => (
              <SettingControl
                key={capability.key}
                capability={capability}
                value={settings[capability.key] ?? capability.default_value}
                onChange={(value) => setSettings((current) => ({ ...current, [capability.key]: value }))}
              />
            ))}
          </div>
          <ErrorNotice message={errors[`config:${runtimeId}`]} />
          <div className="mt-4 flex justify-end">
            <BusyButton type="button" variant="primary" busy={busy[`config:${runtimeId}`]} onClick={() => void saveConfig(runtimeId, settings).catch(() => {})}>
              <Save size={15} aria-hidden="true" /> Save runtime settings
            </BusyButton>
          </div>
        </section>
      )}
    </article>
  );
}

export function RuntimeHubRuntimes() {
  const runtimes = useRuntimeHubStore((state) => state.runtimes);
  const refreshRuntime = useRuntimeHubStore((state) => state.refreshRuntime);
  const runtimeDetails = useRuntimeHubStore((state) => state.runtimeDetails);

  const missing = useMemo(
    () => runtimes.map((runtime) => runtime.descriptor.runtimeId).filter((runtimeId) => !runtimeDetails[runtimeId]),
    [runtimeDetails, runtimes],
  );

  useEffect(() => {
    for (const runtimeId of missing) void refreshRuntime(runtimeId).catch(() => {});
  }, [missing, refreshRuntime]);

  return (
    <div role="tabpanel" id="runtime-hub-panel-runtimes" aria-labelledby="runtime-hub-tab-runtimes" className="flex flex-col gap-4">
      <SectionHeading
        title="Runtime drivers"
        description="Inspect live status, inventory, logs and metrics; load verified models; and apply only capabilities advertised by each driver."
      />
      {runtimes.length ? runtimes.map((runtime) => <RuntimeCard key={runtime.descriptor.runtimeId} runtime={runtime} />) : (
        <div className="rounded-lg border border-dashed border-border p-6 text-center text-sm text-muted">
          No compatible runtime drivers are configured on this system.
        </div>
      )}
    </div>
  );
}
