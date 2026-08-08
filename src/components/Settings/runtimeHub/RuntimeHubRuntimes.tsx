import { useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { Activity, FileText, Gauge, Play, RefreshCw, Save, Square, Wrench } from "lucide-react";
import { Button, StatusPill } from "../../ui";
import type {
  AdvancedSettingCapability,
  ContextCacheView,
  EffectiveContextResolution,
  HardwareSnapshot,
  KeepAlive,
  M3DraftModelCandidate,
  M3InstalledModel,
  M3RuntimeCapability,
  M3RuntimeKind,
  OffloadPlan,
  OffloadPlanInput,
  RunningModel,
  SettingValue,
} from "../../../lib/runtimeHubClient";
import { useRuntimeHubStore, type RuntimeDetail } from "../../../store/runtimeHubStore";
import {
  BusyButton,
  CompatibilityWarningBanner,
  CONTROL_CLASS,
  ErrorNotice,
  Field,
  formatBytes,
  formatDate,
  JsonView,
  labelize,
  ModelRetirementWarningBanner,
  SectionHeading,
  Toggle,
} from "./RuntimeHubShared";
import { errorMessage } from "../../../lib/errors";

function statusState(detail: RuntimeDetail | undefined): string {
  if (!detail?.status) return "unknown";
  if (detail.status.runtimeType === "adapter") return detail.status.status.state;
  return detail.status.status.state;
}

function statusTone(state: string): "neutral" | "success" | "warning" | "danger" {
  if (state === "ready" || state === "running") return "success";
  // `not_installed` is actionable, not broken: the user installs a package and
  // it clears. A neutral pill read as "nothing to see here" next to a card
  // whose only content was an empty model list.
  if (state === "starting" || state === "degraded" || state === "not_installed") return "warning";
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

/** Builds the pure input for `m3_offload_plan` from already-loaded Runtime
 * Hub state: the live hardware snapshot, the model about to be loaded, and
 * every other model currently resident on any runtime (so the plan reflects
 * memory genuinely available right now, not just this runtime's own view). */
export function buildOffloadPlanInput(
  hardware: HardwareSnapshot,
  model: M3InstalledModel,
  runtimes: M3RuntimeCapability[],
  runtimeDetails: Record<string, RuntimeDetail>,
  requestedContextTokens?: number,
): OffloadPlanInput {
  const activeVersion = model.versions.find((version) => version.active);
  const weightsBytes = activeVersion?.sizeBytes ?? model.estimatedRamBytes;
  const others = runtimes
    .flatMap((runtime) => runningModels(runtimeDetails[runtime.descriptor.runtimeId]))
    .filter((resident) => resident.model_id !== model.modelId);
  const reservedRamBytes = others.reduce((sum, resident) => sum + resident.memory_bytes, 0);
  const reservedVramBytes = others.reduce((sum, resident) => sum + resident.vram_bytes, 0);
  // Whether a projector is actually attached — not `model.capabilities.vision`
  // alone (ROADMAP Phase 8 item 12): a model declared vision-capable with no
  // projector reference at all has nothing for this plan to size or place,
  // and the missing-projector warning in the load flow below is what should
  // surface that gap, not a phantom memory reservation here.
  const hasVisionProjector = activeVersion?.projector != null;
  const projectorMemoryBytes = activeVersion?.estimatedProjectorMemoryBytes ?? 0;
  return {
    hardware,
    model: {
      weights_bytes: weightsBytes,
      estimated_ram_bytes: model.estimatedRamBytes,
      estimated_vram_bytes: model.estimatedVramBytes,
      required_accelerator: (model.requiredAccelerator as OffloadPlanInput["model"]["required_accelerator"]) ?? null,
      has_vision_projector: hasVisionProjector,
      projector_memory_bytes: hasVisionProjector ? projectorMemoryBytes : 0,
    },
    reserved: { ram_bytes: reservedRamBytes, vram_bytes: reservedVramBytes },
    other_resident_count: others.length,
    requested_context_tokens: requestedContextTokens ?? null,
  };
}

/** A clear, actionable warning when the selected model's active version
 * declares a projector-requiring capability (vision) but that projector is
 * missing or not yet locally verified (ROADMAP Phase 8 item 12) — surfaced
 * near the load flow so a load is never attempted silently against a model
 * that will fail or behave incorrectly without a working vision component.
 * `null` when nothing needs surfacing (no active version, no projector
 * requirement, or an already-verified projector). */
export function missingProjectorWarning(model: M3InstalledModel): string | null {
  const activeVersion = model.versions.find((version) => version.active);
  if (!activeVersion) return null;
  if (activeVersion.projectorVerification === "missing_reference") {
    return `${model.displayName} is declared vision-capable but has no associated multimodal projector. Loading it will not provide working image understanding until a projector is added to its manifest.`;
  }
  if (activeVersion.projectorVerification === "unverified") {
    return `${model.displayName}'s multimodal projector (${activeVersion.projector?.kind ?? "unknown"}) has not been verified locally yet. Verify it on the Models tab before relying on vision support.`;
  }
  return null;
}

const PROJECTOR_PLACEMENT_LABEL: Record<OffloadPlan["projector_placement"], string> = {
  gpu: "GPU",
  cpu: "CPU",
  not_applicable: "N/A",
};

function OffloadPlanPanel({
  plan,
  busy,
  error,
}: {
  plan: OffloadPlan | undefined;
  busy: boolean | undefined;
  error: string | undefined;
}) {
  if (!plan && !busy && !error) return null;
  return (
    <div className="mt-3 rounded-md border border-border bg-surface-2 p-3" aria-live="polite">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-xs font-semibold text-foreground">Offload plan</p>
        {busy && <StatusPill tone="neutral">Computing…</StatusPill>}
      </div>
      <ErrorNotice message={error} />
      {plan && (
        <>
          <div className="mt-2 grid gap-2 text-xs text-muted sm:grid-cols-3">
            <span>Accelerator: {labelize(plan.accelerator)}</span>
            <span>
              Context: {plan.context_tokens.toLocaleString()} tokens
              {plan.context_tokens < plan.requested_context_tokens
                ? ` (requested ${plan.requested_context_tokens.toLocaleString()})`
                : ""}
            </span>
            <span>Batch size: {plan.batch_size}</span>
            <span>GPU layers: {plan.gpu_layers} / {plan.estimated_total_layers}</span>
            <span>CPU spill: {plan.cpu_spill_layers} layer{plan.cpu_spill_layers === 1 ? "" : "s"}</span>
            <span>Parallel sequences: {plan.parallel_sequences}</span>
            {plan.projector_placement !== "not_applicable" && (
              <span>Projector: {PROJECTOR_PLACEMENT_LABEL[plan.projector_placement]}</span>
            )}
          </div>
          {plan.rationale.length > 0 && (
            <ul className="mt-3 list-disc space-y-1 pl-5 text-xs leading-5 text-muted">
              {plan.rationale.map((entry) => <li key={entry.field}>{entry.explanation}</li>)}
            </ul>
          )}
          {plan.improvement_suggestions.length > 0 && (
            <div className="mt-3 rounded-md border border-warning/30 bg-warning-soft p-2">
              <p className="text-xs font-medium text-warning">How to improve this plan</p>
              <ul className="mt-1 list-disc space-y-1 pl-5 text-xs leading-5 text-warning">
                {plan.improvement_suggestions.map((suggestion) => <li key={suggestion}>{suggestion}</li>)}
              </ul>
            </div>
          )}
        </>
      )}
    </div>
  );
}

/** One-line, honest summary of a runtime's configured context size: prefers
 * a value the runtime itself confirmed live over one this app merely
 * requested, and says so — never presents an estimate as a guaranteed fact. */
export function contextCacheHeadline(view: ContextCacheView): string {
  const tokens = view.reportedContextTokens ?? view.configured.tokens;
  if (tokens == null) return "Context size unavailable for this runtime.";
  const sourceLabel =
    view.reportedContextTokens != null
      ? "confirmed live by the runtime"
      : view.configured.source === "runtime_configured"
        ? "configured by this app"
        : view.configured.source === "runtime_default"
          ? "the runtime's default"
          : "unavailable";
  return `${tokens.toLocaleString()} tokens (${sourceLabel})`;
}

function ContextCachePanel({ view }: { view: ContextCacheView | undefined }) {
  if (!view) return null;
  return (
    <div className="mt-3 rounded-md border border-border bg-surface-2 p-3">
      <p className="text-xs font-semibold text-foreground">Context & cache</p>
      <div className="mt-2 grid gap-2 text-xs text-muted sm:grid-cols-2">
        <span>Context size: {contextCacheHeadline(view)}</span>
        {view.contextTokensInUse != null && (
          <span>Tokens in use: {view.contextTokensInUse.toLocaleString()}</span>
        )}
        {view.contextHeadroomTokens != null && (
          <span>Headroom: {view.contextHeadroomTokens.toLocaleString()} tokens</span>
        )}
        {view.contextShiftDetected != null && (
          <span>
            Context shift: {view.contextShiftDetected ? "detected — earlier turns may have been dropped" : "not detected"}
          </span>
        )}
        {view.totalSlots != null && <span>Server slots: {view.totalSlots}</span>}
      </div>
      {/* Both arms render, because "this runtime cannot share a prefix" is as
          useful to know as that it can — and the union makes it impossible to
          show either verdict without the sentence that justifies it. */}
      <p className="mt-2 text-xs leading-5 text-muted">
        <span className="font-medium text-foreground">
          {view.prefixSharing.state === "supported"
            ? "Prompt prefixes are shared between processes: "
            : "Prompt prefixes are not shared between processes: "}
        </span>
        {view.prefixSharing.state === "supported"
          ? view.prefixSharing.mechanism
          : view.prefixSharing.reason}
      </p>
      {/* Only when a budget cannot be enforced. There is nothing to say when it
          can — the enforcement is silent and correct — but "you can set a limit
          here and it will do nothing" is exactly what a user must not discover
          by setting one. */}
      {view.contextBudget.state === "unenforceable" && (
        <p className="mt-1 text-xs leading-5 text-warning">
          A per-process context budget cannot be enforced on this runtime: {view.contextBudget.reason}
        </p>
      )}
      {view.notes.length > 0 && (
        <ul className="mt-3 list-disc space-y-1 pl-5 text-xs leading-5 text-muted">
          {view.notes.map((note) => (
            <li key={note}>{note}</li>
          ))}
        </ul>
      )}
    </div>
  );
}

function EffectiveContextPanel({
  runtime,
  offloadPlan,
}: {
  runtime: M3RuntimeCapability;
  offloadPlan: OffloadPlan | undefined;
}) {
  const resolveEffectiveContext = useRuntimeHubStore((state) => state.resolveEffectiveContext);
  const contextSetting = runtime.settings.find((setting) => setting.key === "context_size" || setting.key === "num_ctx");
  const schemaBounds = contextSetting?.schema.type === "integer" ? contextSetting.schema : undefined;
  const [requested, setRequested] = useState(() => offloadPlan?.context_tokens ?? schemaBounds?.max ?? 4_096);
  const [resolution, setResolution] = useState<EffectiveContextResolution | undefined>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | undefined>();

  if (!contextSetting || !offloadPlan) return null;

  async function handleResolve() {
    setBusy(true);
    setError(undefined);
    try {
      const result = await resolveEffectiveContext({
        requestedTokens: requested,
        offloadPlanContextTokens: offloadPlan?.context_tokens ?? requested,
        modelMetadataMaxContextTokens: null,
        runtimeSettingMinTokens: schemaBounds?.min ?? null,
        runtimeSettingMaxTokens: schemaBounds?.max ?? null,
      });
      setResolution(result);
    } catch (thrown) {
      setError(errorMessage(thrown));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="mt-3 rounded-md border border-border bg-surface-2 p-3">
      <p className="text-xs font-semibold text-foreground">Effective context size</p>
      <p className="mt-1 text-xs text-muted">
        Preview what a requested context size resolves to once bounded by the offload plan and this runtime&apos;s
        configured limits, without loading anything.
      </p>
      <div className="mt-2 flex flex-wrap items-end gap-2">
        <Field label="Requested tokens">
          <input
            type="number"
            min={schemaBounds?.min}
            max={schemaBounds?.max}
            value={requested}
            onChange={(event) => setRequested(Number(event.target.value))}
            className={CONTROL_CLASS}
          />
        </Field>
        <BusyButton type="button" busy={busy} onClick={() => void handleResolve()}>
          Check
        </BusyButton>
      </div>
      <ErrorNotice message={error} />
      {resolution && (
        <div className="mt-2 text-xs text-muted">
          <p className="font-medium text-foreground">Effective: {resolution.effectiveTokens.toLocaleString()} tokens</p>
          {resolution.rationale.length > 0 && (
            <ul className="mt-1 list-disc space-y-1 pl-5 leading-5">
              {resolution.rationale.map((entry) => (
                <li key={entry}>{entry}</li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}

/** Builds a `Field`'s hint text: the capability's description, a restart
 * note, and — when this control can't actually be enabled right now — the
 * reason why, so a disabled control is never left unexplained. */
export function settingHint(capability: AdvancedSettingCapability, extra?: string): string {
  const restartNote = capability.restart_required ? " Restart required." : "";
  const unsupportedNote =
    !capability.supported && capability.unsupported_reason ? ` ${capability.unsupported_reason}` : "";
  return `${capability.description}${extra ?? ""}${restartNote}${unsupportedNote}`;
}

function SettingControl({
  capability,
  value,
  onChange,
  draftModelCandidates,
}: {
  capability: AdvancedSettingCapability;
  value: SettingValue;
  onChange: (value: SettingValue) => void;
  draftModelCandidates?: M3DraftModelCandidate[];
}) {
  const schema = capability.schema;
  const disabled = !capability.supported;
  const label = disabled ? (
    <span className="inline-flex items-center gap-1.5">
      {capability.label}
      <StatusPill tone="neutral">Unavailable</StatusPill>
    </span>
  ) : (
    capability.label
  );
  if (schema.type === "boolean" && value.type === "boolean") {
    return (
      <Toggle
        checked={value.value}
        onChange={(next) => onChange({ type: "boolean", value: next })}
        label={capability.label}
        description={settingHint(capability)}
        disabled={disabled}
      />
    );
  }
  if (schema.type === "choice" && value.type === "choice") {
    return (
      <Field label={label} hint={settingHint(capability)}>
        <select
          value={value.value}
          onChange={(event) => onChange({ type: "choice", value: event.target.value })}
          className={CONTROL_CLASS}
          disabled={disabled}
        >
          {schema.options.map((option) => <option key={option} value={option}>{option}</option>)}
        </select>
      </Field>
    );
  }
  if (schema.type === "text" && value.type === "text" && capability.key === "speculative_decoding_draft_model") {
    const candidates = draftModelCandidates ?? [];
    return (
      <Field label={label} hint={settingHint(capability)}>
        <select
          value={value.value}
          onChange={(event) => onChange({ type: "text", value: event.target.value })}
          className={CONTROL_CLASS}
          disabled={disabled || candidates.length === 0}
        >
          <option value="">None (disabled)</option>
          {candidates.map((candidate) => (
            <option key={candidate.modelId} value={candidate.modelId}>
              {candidate.displayName}
            </option>
          ))}
        </select>
      </Field>
    );
  }
  if (schema.type === "text" && value.type === "text") {
    return (
      <Field label={label} hint={settingHint(capability, ` Up to ${schema.max_bytes} bytes.`)}>
        <input
          value={value.value}
          onChange={(event) => onChange({ type: "text", value: event.target.value })}
          className={CONTROL_CLASS}
          disabled={disabled}
        />
      </Field>
    );
  }
  if (schema.type === "integer" && value.type === "integer") {
    return (
      <Field label={label} hint={settingHint(capability)}>
        <input
          type="number"
          min={schema.min}
          max={schema.max}
          step={schema.step}
          value={value.value}
          onChange={(event) => onChange({ type: "integer", value: Number(event.target.value) })}
          className={CONTROL_CLASS}
          disabled={disabled}
        />
      </Field>
    );
  }
  if (schema.type === "float" && value.type === "float") {
    return (
      <Field label={label} hint={settingHint(capability)}>
        <input
          type="number"
          min={schema.min}
          max={schema.max}
          step={schema.step}
          value={value.value}
          onChange={(event) => onChange({ type: "float", value: Number(event.target.value) })}
          className={CONTROL_CLASS}
          disabled={disabled}
        />
      </Field>
    );
  }
  if (schema.type === "duration_ms" && value.type === "duration_ms") {
    return (
      <Field label={label} hint={settingHint(capability, " Milliseconds.")}>
        <input
          type="number"
          min={schema.min}
          max={schema.max}
          step={schema.step}
          value={value.value}
          onChange={(event) => onChange({ type: "duration_ms", value: Number(event.target.value) })}
          className={CONTROL_CLASS}
          disabled={disabled}
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
  const hardware = useRuntimeHubStore((state) => state.hardware);
  const allRuntimes = useRuntimeHubStore((state) => state.runtimes);
  const runtimeDetails = useRuntimeHubStore((state) => state.runtimeDetails);
  const offloadPlan = useRuntimeHubStore((state) => state.offloadPlans[runtimeId]);
  const previewOffloadPlan = useRuntimeHubStore((state) => state.previewOffloadPlan);
  const installMlxPackage = useRuntimeHubStore((state) => state.installMlxPackage);
  const offloadBusy = busy[`offload-plan:${runtimeId}`];
  const offloadError = errors[`offload-plan:${runtimeId}`];
  const compatibilityReport = useRuntimeHubStore((state) => state.compatibilityReport);
  const modelStalenessWarnings = useRuntimeHubStore((state) => state.modelStalenessWarnings);
  const checkModelStaleness = useRuntimeHubStore((state) => state.checkModelStaleness);
  const settingCapabilities = useRuntimeHubStore((state) => state.settingCapabilities[runtimeId]);
  const resolveSettingCapabilities = useRuntimeHubStore((state) => state.resolveSettingCapabilities);

  const compatibleModels = installedModels.filter((model) => model.runtime === runtime.descriptor.kind);
  const [assetId, setAssetId] = useState(compatibleModels[0]?.assetId ?? "");
  const [replaceExisting, setReplaceExisting] = useState(false);
  const [keepAliveMode, setKeepAliveMode] = useState<"duration" | "forever">("duration");
  const [keepAliveMinutes, setKeepAliveMinutes] = useState(10);
  const [forceByModel, setForceByModel] = useState<Record<string, boolean>>({});
  const [showLogs, setShowLogs] = useState(false);
  const [showMetrics, setShowMetrics] = useState(false);
  const [showSettings, setShowSettings] = useState(false);
  const [showContextCache, setShowContextCache] = useState(false);
  const [settings, setSettings] = useState<Record<string, SettingValue>>(() =>
    Object.fromEntries(runtime.settings.map((setting) => [setting.key, setting.default_value])),
  );

  useEffect(() => {
    if (!assetId && compatibleModels[0]) setAssetId(compatibleModels[0].assetId);
  }, [assetId, compatibleModels]);

  useEffect(() => {
    if (detail?.config) setSettings((current) => ({ ...current, ...detail.config }));
  }, [detail?.config]);

  const selectedModel = compatibleModels.find((model) => model.assetId === assetId);
  useEffect(() => {
    if (!hardware || !selectedModel) return;
    const input = buildOffloadPlanInput(hardware, selectedModel, allRuntimes, runtimeDetails);
    void previewOffloadPlan(runtimeId, input).catch(() => {});
  }, [hardware, selectedModel, allRuntimes, runtimeDetails, runtimeId, previewOffloadPlan]);

  // Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item
  // 14): check before a load actually starts, same trigger point as the
  // offload-plan preview above.
  useEffect(() => {
    if (!selectedModel) return;
    void checkModelStaleness(selectedModel.assetId).catch(() => {});
  }, [selectedModel, checkModelStaleness]);

  // Sampler/Batching/Speculative Decoding Controls (ROADMAP Phase 8 item
  // 17): re-resolve which advanced settings can actually be enabled
  // whenever the runtime or selected model changes. Runs even with no model
  // selected (`selectedModel?.assetId ?? null`) so hardware-only gates
  // (flash attention, mixed precision) are visible immediately.
  useEffect(() => {
    void resolveSettingCapabilities(runtimeId, selectedModel?.assetId ?? null).catch(() => {});
  }, [runtimeId, selectedModel?.assetId, resolveSettingCapabilities]);

  const gatedSettings = settingCapabilities?.settings ?? runtime.settings;
  const draftModelCandidates = settingCapabilities?.draftModelCandidates ?? [];

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

      {/* MLX is the one runtime the app does not ship: its service package is
          installed separately and Ed25519-verified against the pinned release
          key. Until one is installed there is nothing to load a model into, so
          the card offers the install rather than an empty model picker. */}
      {state === "not_installed" && (
        <div className="mt-4 rounded-md border border-border bg-surface-2 p-3">
          <p className="text-sm text-foreground">No MLX service package is installed.</p>
          <p className="mt-1 text-xs text-muted">
            Build one with <code className="font-mono">pnpm mlx:package</code>, then choose the
            resulting folder. It is only installed if the pinned release key signed it.
          </p>
          <BusyButton
            type="button"
            className="mt-3"
            busy={busy["mlx-install"]}
            onClick={() =>
              void open({ directory: true, multiple: false }).then((path) => {
                if (typeof path === "string") void installMlxPackage(path).catch(() => {});
              })
            }
          >
            Choose package folder…
          </BusyButton>
          <ErrorNotice message={errors["mlx-install"]} />
        </div>
      )}

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
          <div className="mt-3 flex flex-col gap-3">
            <CompatibilityWarningBanner report={compatibilityReport} />
            <ModelRetirementWarningBanner warning={selectedModel ? modelStalenessWarnings[selectedModel.assetId] : undefined} />
            <ErrorNotice message={selectedModel ? errors[`model-staleness:${selectedModel.assetId}`] : undefined} />
          </div>
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
          {selectedModel && missingProjectorWarning(selectedModel) && (
            <div className="mt-3 rounded-md border border-warning/30 bg-warning-soft p-3" role="alert">
              <p className="text-xs leading-5 text-warning">{missingProjectorWarning(selectedModel)}</p>
            </div>
          )}
          <OffloadPlanPanel plan={offloadPlan} busy={offloadBusy} error={offloadError} />
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
        {detail?.contextCache && (
          <Button
            type="button"
            className="min-h-11"
            aria-expanded={showContextCache}
            onClick={() => setShowContextCache((value) => !value)}
          >
            <Gauge size={15} aria-hidden="true" /> {showContextCache ? "Hide context & cache" : "Context & cache"}
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
      {showContextCache && detail?.contextCache && (
        <div className="mt-4">
          <ContextCachePanel view={detail.contextCache} />
          <EffectiveContextPanel runtime={runtime} offloadPlan={offloadPlan} />
        </div>
      )}

      {showSettings && runtime.settings.length > 0 && (
        <section className="mt-4 rounded-lg border border-border bg-surface p-4" aria-label={`Advanced settings for ${runtime.descriptor.label}`}>
          <div className="grid gap-4 sm:grid-cols-2">
            {gatedSettings.map((capability) => (
              <SettingControl
                key={capability.key}
                capability={capability}
                value={settings[capability.key] ?? capability.default_value}
                onChange={(value) => setSettings((current) => ({ ...current, [capability.key]: value }))}
                draftModelCandidates={draftModelCandidates}
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
