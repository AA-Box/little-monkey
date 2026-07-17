import { useState, type FormEvent } from "react";
import { ArchiveRestore, Download, Eraser, ExternalLink, GitCompareArrows, RefreshCw, Search, ShieldCheck, Trash2, X } from "lucide-react";
import { Button, StatusPill } from "../../ui";
import type {
  AcceleratorKind,
  HardwareProfile,
  HardwareSnapshot,
  M3CatalogMatch,
  M3InstalledModel,
  M3InstalledVersion,
  M3RuntimeCapability,
  M3SchedulingInput,
  SchedulerRuntimeKind,
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
  labelize,
  SectionHeading,
} from "./RuntimeHubShared";

function assetIdFor(match: M3CatalogMatch): string {
  return `${match.model.runtime}:${match.model.modelId}:${match.model.variantId}`;
}

function FitPill({ match }: { match: M3CatalogMatch }) {
  const tone =
    match.fit.rating === "recommended"
      ? "success"
      : match.fit.rating === "tight"
        ? "warning"
        : "danger";
  return <StatusPill tone={tone}>{labelize(match.fit.rating)}</StatusPill>;
}

function CapabilityList({ capabilities }: { capabilities: M3CatalogMatch["model"]["capabilities"] }) {
  const entries = Object.entries(capabilities).filter(([, enabled]) => enabled);
  return (
    <div className="flex flex-wrap gap-1.5" aria-label="Model capabilities">
      {entries.map(([capability]) => (
        <span key={capability} className="rounded-md bg-surface-2 px-2 py-1 text-xs text-muted">
          {labelize(capability)}
        </span>
      ))}
    </div>
  );
}

function CatalogCard({ match }: { match: M3CatalogMatch }) {
  const installedModels = useRuntimeHubStore((state) => state.installedModels);
  const busy = useRuntimeHubStore((state) => state.busy);
  const errors = useRuntimeHubStore((state) => state.errors);
  const progress = useRuntimeHubStore((state) => state.downloadProgress);
  const downloadModel = useRuntimeHubStore((state) => state.downloadModel);
  const updateModel = useRuntimeHubStore((state) => state.updateModel);
  const cancelOperation = useRuntimeHubStore((state) => state.cancelOperation);
  const [accepted, setAccepted] = useState(false);

  const assetId = assetIdFor(match);
  const installed = installedModels.find((model) => model.assetId === assetId);
  const activeVersion = installed?.versions.find((version) => version.active);
  const alreadyCurrent = activeVersion?.revision === match.model.revision && activeVersion.sha256 === match.model.sha256;
  const operationKey = installed ? `update:${assetId}` : `download:${assetId}`;
  const currentProgress = progress[assetId];
  const percent = currentProgress?.totalBytes
    ? Math.min(100, (currentProgress.downloadedBytes / currentProgress.totalBytes) * 100)
    : 0;

  return (
    <article className="rounded-lg border border-border bg-background p-4 transition-colors hover:border-border-strong">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <h4 className="break-words text-sm font-semibold text-foreground">{match.model.displayName}</h4>
            <FitPill match={match} />
          </div>
          <p className="mt-1 break-all font-mono text-xs text-muted">
            {match.model.modelId} · {match.model.variantId} · {match.model.revision}
          </p>
        </div>
        <span className="rounded-md border border-border px-2 py-1 font-mono text-xs text-muted">
          {labelize(match.model.runtime)}
        </span>
      </div>

      <div className="mt-3 grid gap-2 text-xs text-muted sm:grid-cols-3">
        <span>{formatBytes(match.model.sizeBytes)} download</span>
        <span>{formatBytes(match.model.estimatedRamBytes)} estimated RAM</span>
        <span>{match.model.quantization ?? "Unquantized"}</span>
      </div>
      {(match.model.template || match.model.projector) && (
        <div className="mt-2 flex flex-wrap gap-1.5 text-xs text-muted">
          {match.model.template && (
            <span className="rounded-md bg-surface-2 px-2 py-1">Template {match.model.template}</span>
          )}
          {match.model.projector && (
            <span className="rounded-md bg-surface-2 px-2 py-1">
              Projector {match.model.projector.kind} ({formatBytes(match.model.projector.sizeBytes)})
            </span>
          )}
        </div>
      )}
      <div className="mt-3 grid gap-2 rounded-md border border-border bg-surface-2 p-3 text-xs text-muted sm:grid-cols-2">
        <span>RAM: {formatBytes(match.fit.requiredRamBytes)} required / {formatBytes(match.fit.availableRamBytes)} schedulable</span>
        <span>VRAM: {formatBytes(match.fit.requiredVramBytes)} required / {formatBytes(match.fit.availableVramBytes)} available</span>
      </div>
      <div className="mt-3"><CapabilityList capabilities={match.model.capabilities} /></div>

      {match.fit.reasons.length > 0 && (
        <ul className="mt-3 list-disc space-y-1 pl-5 text-xs leading-5 text-muted">
          {match.fit.reasons.map((reason) => <li key={reason}>{reason}</li>)}
        </ul>
      )}

      <div className="mt-4 rounded-md border border-border bg-surface-2 p-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div>
            <p className="text-xs font-medium text-foreground">{match.model.license.name}</p>
            <p className="mt-0.5 text-xs text-muted">
              {match.model.license.spdxId ?? "No SPDX id"} · declaration revision {match.model.license.revision}
            </p>
          </div>
          <a
            href={match.model.license.sourceUrl}
            target="_blank"
            rel="noreferrer"
            className="inline-flex min-h-11 items-center gap-1.5 rounded-md px-2 text-xs text-accent hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
          >
            License source <ExternalLink size={13} aria-hidden="true" />
          </a>
        </div>
        {!alreadyCurrent && (
          <label className="mt-2 flex min-h-11 cursor-pointer items-center gap-2 text-xs text-foreground">
            <input
              type="checkbox"
              checked={accepted}
              onChange={(event) => setAccepted(event.target.checked)}
              className="h-4 w-4 rounded border-border accent-[var(--color-accent)] focus-visible:ring-2 focus-visible:ring-accent"
            />
            I accept this exact license declaration and revision.
          </label>
        )}
      </div>

      {currentProgress && (
        <div className="mt-4" aria-live="polite">
          <div className="flex justify-between gap-2 text-xs text-muted">
            <span>{labelize(currentProgress.phase)}</span>
            <span>{formatBytes(currentProgress.downloadedBytes)} / {formatBytes(currentProgress.totalBytes)}</span>
          </div>
          <div
            role="progressbar"
            aria-label={`Download ${match.model.displayName}`}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={Math.round(percent)}
            className="mt-2 h-2 overflow-hidden rounded-full bg-surface-2"
          >
            <div
              className="h-full rounded-full bg-accent transition-[width] motion-reduce:transition-none"
              style={{ width: `${Math.max(2, percent)}%` }}
            />
          </div>
        </div>
      )}

      <ErrorNotice message={errors[operationKey]} />
      <div className="mt-4 flex flex-wrap justify-end gap-2">
        {currentProgress ? (
          <Button
            type="button"
            variant="danger"
            className="min-h-11"
            onClick={() => void cancelOperation(operationKey).catch(() => {})}
            disabled={currentProgress.phase === "cancelling"}
          >
            <X size={15} aria-hidden="true" /> Cancel
          </Button>
        ) : alreadyCurrent ? (
          <Button type="button" className="min-h-11" disabled>
            <ShieldCheck size={15} aria-hidden="true" /> Installed
          </Button>
        ) : installed ? (
          <BusyButton
            type="button"
            variant="primary"
            busy={busy[operationKey]}
            disabled={!accepted || match.fit.rating === "incompatible"}
            onClick={() => void updateModel(assetId, match).catch(() => {})}
          >
            <RefreshCw size={15} aria-hidden="true" /> Update
          </BusyButton>
        ) : (
          <BusyButton
            type="button"
            variant="primary"
            busy={busy[operationKey]}
            disabled={!accepted || match.fit.rating === "incompatible"}
            onClick={() => void downloadModel(match).catch(() => {})}
          >
            <Download size={15} aria-hidden="true" /> Download
          </BusyButton>
        )}
      </div>
    </article>
  );
}

const SCHEDULER_ACCELERATORS = new Set<AcceleratorKind>(["cpu", "metal", "cuda", "rocm", "vulkan", "direct_ml"]);

export function buildSchedulingInput(
  hardware: HardwareSnapshot,
  profile: HardwareProfile,
  runtimes: M3RuntimeCapability[],
  runtimeDetails: Record<string, RuntimeDetail>,
  installedModels: M3InstalledModel[],
  selectedAssetIds: string[],
): M3SchedulingInput {
  const eligibleRuntimes = runtimes.filter((runtime) => runtime.descriptor.kind !== "mlx");
  const residentRecords = eligibleRuntimes.flatMap((runtime) => {
    const detail = runtimeDetails[runtime.descriptor.runtimeId];
    if (!detail?.status || detail.status.runtimeType !== "adapter") return [];
    return detail.status.running_models.map((resident) => ({ runtime, resident }));
  });
  const processSlots = eligibleRuntimes.flatMap((runtime) => {
    const runtimeResidents = residentRecords.filter((record) => record.runtime.descriptor.runtimeId === runtime.descriptor.runtimeId);
    const slotCount = runtime.descriptor.kind === "llama_cpp"
      ? 1
      : Math.max(1, profile.recommended_process_slots, runtimeResidents.length);
    return Array.from({ length: slotCount }, (_, index) => {
      const resident = runtimeResidents[index]?.resident;
      return {
        slot_id: runtime.descriptor.kind === "llama_cpp" ? runtime.descriptor.runtimeId : `${runtime.descriptor.runtimeId}-${index + 1}`,
        runtime: runtime.descriptor.kind as SchedulerRuntimeKind,
        port: null,
        state: resident
          ? { state: "occupied" as const, model_id: resident.model_id, ownership: resident.ownership }
          : { state: "available" as const },
      };
    });
  });
  const residents = residentRecords.map(({ runtime, resident }) => ({
    runtime: runtime.descriptor.kind as SchedulerRuntimeKind,
    model_id: resident.model_id,
    memory: { ram_bytes: resident.memory_bytes, vram_bytes: resident.vram_bytes },
    ownership: resident.ownership,
    slot_id: runtime.descriptor.kind === "llama_cpp" ? runtime.descriptor.runtimeId : null,
    port: null,
  }));
  const selected = new Set(selectedAssetIds);
  const targets = installedModels
    .filter((model) => selected.has(model.assetId) && model.runtime !== "mlx")
    .map((model) => ({
      target_id: model.assetId,
      runtime: model.runtime as SchedulerRuntimeKind,
      model_id: model.modelId,
      memory: { ram_bytes: model.estimatedRamBytes, vram_bytes: model.estimatedVramBytes },
      accelerator: model.requiredAccelerator && SCHEDULER_ACCELERATORS.has(model.requiredAccelerator as AcceleratorKind)
        ? model.requiredAccelerator as AcceleratorKind
        : null,
      preferred_slot_id: null,
    }));
  const availableVram = Math.max(
    0,
    ...hardware.platform.accelerators
      .filter((accelerator) => accelerator.available)
      .map((accelerator) => accelerator.available_memory_bytes ?? 0),
  );
  return {
    platform: hardware.platform,
    memory: {
      available_ram_bytes: hardware.available_ram_bytes,
      reserve_ram_bytes: Math.min(profile.recommended_ram_reserve_bytes, hardware.available_ram_bytes),
      available_vram_bytes: availableVram,
      reserve_vram_bytes: 0,
    },
    process_slots: processSlots,
    residents,
    ports: [],
    targets,
  };
}

const PROJECTOR_VERIFICATION_TONE: Record<M3InstalledVersion["projectorVerification"], "neutral" | "success" | "warning" | "danger"> = {
  not_required: "neutral",
  missing_reference: "danger",
  unverified: "warning",
  verified: "success",
};

const PROJECTOR_VERIFICATION_LABEL: Record<M3InstalledVersion["projectorVerification"], string> = {
  not_required: "No projector required",
  missing_reference: "Missing projector",
  unverified: "Projector unverified",
  verified: "Projector verified",
};

/** Inline provenance/placement/capability evidence for one installed
 * version's multimodal projector (ROADMAP Phase 8 item 12), plus a form to
 * promote it from "declared" to genuinely `verified` by pointing at a local
 * file that should match the manifest's declared digest/size. There is
 * deliberately no download button here: `M3ProjectorRef` carries no fetch
 * URL yet, so a user (or a future PR) supplies the bytes. */
function ProjectorEvidence({ assetId, version }: { assetId: string; version: M3InstalledVersion }) {
  const verifyProjector = useRuntimeHubStore((state) => state.verifyProjector);
  const busy = useRuntimeHubStore((state) => state.busy[`verify-projector:${assetId}`]);
  const error = useRuntimeHubStore((state) => state.errors[`verify-projector:${assetId}`]);
  const [candidatePath, setCandidatePath] = useState("");
  // Nothing to show for the common case: not vision-capable and no
  // projector reference at all. A projector present despite not being
  // "required" (an inconsistent manifest) still gets its evidence shown.
  if (version.projectorVerification === "not_required" && !version.projector) return null;

  return (
    <div className="mt-2 rounded-md border border-border bg-surface p-2">
      <div className="flex flex-wrap items-center gap-2">
        {version.projectorVerification !== "not_required" && (
          <StatusPill tone={PROJECTOR_VERIFICATION_TONE[version.projectorVerification]}>
            {PROJECTOR_VERIFICATION_LABEL[version.projectorVerification]}
          </StatusPill>
        )}
        {version.visionReady && <StatusPill tone="success">Vision ready</StatusPill>}
        {version.projector && (
          <span className="font-mono text-[11px] text-muted">
            {version.projector.kind} · sha256 {version.projector.sha256.slice(0, 12)}… ·{" "}
            {formatBytes(version.estimatedProjectorMemoryBytes ?? version.projector.sizeBytes)} estimated memory
          </span>
        )}
      </div>
      {version.projectorVerification === "missing_reference" && (
        <p className="mt-1.5 text-[11px] leading-5 text-danger">
          This version declares vision capability but its manifest has no associated projector reference at all.
        </p>
      )}
      {version.projector && version.projectorVerification === "unverified" && (
        <form
          className="mt-2 flex flex-wrap items-center gap-2"
          onSubmit={(event: FormEvent) => {
            event.preventDefault();
            if (!candidatePath.trim()) return;
            void verifyProjector(assetId, version.versionKey, candidatePath.trim()).catch(() => {});
          }}
        >
          <input
            value={candidatePath}
            onChange={(changeEvent) => setCandidatePath(changeEvent.target.value)}
            placeholder="/path/to/projector-file"
            aria-label={`Local projector file path to verify for ${version.revision}`}
            className={`${CONTROL_CLASS} min-w-[16rem] flex-1 font-mono text-xs`}
          />
          <BusyButton type="submit" busy={busy} disabled={!candidatePath.trim()}>
            <ShieldCheck size={14} aria-hidden="true" /> Verify projector
          </BusyButton>
        </form>
      )}
      <ErrorNotice message={error} />
    </div>
  );
}

function InstalledCard({ model }: { model: M3InstalledModel }) {
  const setCatalogQuery = useRuntimeHubStore((state) => state.setCatalogQuery);
  const searchCatalog = useRuntimeHubStore((state) => state.searchCatalog);
  const activateVersion = useRuntimeHubStore((state) => state.activateModelVersion);
  const pruneVersions = useRuntimeHubStore((state) => state.pruneModelVersions);
  const deleteModel = useRuntimeHubStore((state) => state.deleteModel);
  const busyState = useRuntimeHubStore((state) => state.busy);
  const errors = useRuntimeHubStore((state) => state.errors);
  const [confirming, setConfirming] = useState(false);
  const [confirmation, setConfirmation] = useState("");
  const [confirmingPrune, setConfirmingPrune] = useState(false);
  const [pruneConfirmation, setPruneConfirmation] = useState("");
  const expected = `DELETE ${model.assetId}`;
  const expectedPrune = `PRUNE ${model.assetId}`;
  const active = model.versions.find((version) => version.active);

  function findUpdates() {
    setCatalogQuery(model.modelId);
    void searchCatalog(model.modelId).catch(() => {});
  }

  return (
    <article className="rounded-lg border border-border bg-background p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h4 className="break-words text-sm font-semibold text-foreground">{model.displayName}</h4>
          <p className="mt-1 break-all font-mono text-xs text-muted">{model.assetId}</p>
        </div>
        <StatusPill tone="success">Verified</StatusPill>
      </div>
      <div className="mt-3 grid gap-2 text-xs text-muted sm:grid-cols-2">
        <span>{model.versions.length} installed version{model.versions.length === 1 ? "" : "s"}</span>
        <span>{formatBytes(active?.sizeBytes)} active</span>
        <span>Revision {active?.revision ?? "unknown"}</span>
        <span>{active?.license.name ?? "License unavailable"}</span>
      </div>
      <div className="mt-4 flex flex-col gap-2" aria-label={`Installed versions of ${model.displayName}`}>
        {model.versions
          .slice()
          .sort((left, right) => right.installedAtMs - left.installedAtMs)
          .map((version) => (
            <div key={version.versionKey} className="rounded-md border border-border bg-surface-2 p-3">
              <div className="flex flex-wrap items-center justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <p className="text-xs font-medium text-foreground">Revision {version.revision}</p>
                    {version.active && <StatusPill tone="success">Active</StatusPill>}
                  </div>
                  <p className="mt-1 break-all font-mono text-[11px] text-muted">
                    {version.versionKey.slice(0, 16)}… · {formatBytes(version.sizeBytes)} · {formatDate(version.installedAtMs)}
                  </p>
                  <p className="mt-1 break-all text-[11px] text-muted">
                    {[
                      `Source ${version.sourceId}`,
                      version.template ? `Template ${version.template}` : null,
                      version.catalogRetrievedAtMs ? `Catalog retrieved ${formatDate(version.catalogRetrievedAtMs)}` : null,
                    ]
                      .filter(Boolean)
                      .join(" · ")}
                  </p>
                </div>
                {!version.active && (
                  <BusyButton
                    type="button"
                    busy={busyState[`activate-version:${model.assetId}`]}
                    onClick={() => void activateVersion(model.assetId, version.versionKey).catch(() => {})}
                  >
                    <ArchiveRestore size={15} aria-hidden="true" /> Roll back to this version
                  </BusyButton>
                )}
              </div>
              <ProjectorEvidence assetId={model.assetId} version={version} />
            </div>
          ))}
      </div>
      <ErrorNotice message={errors[`activate-version:${model.assetId}`]} />
      <ErrorNotice message={errors[`prune:${model.assetId}`]} />
      <ErrorNotice message={errors[`delete:${model.assetId}`]} />

      {confirmingPrune && (
        <div className="mt-4 rounded-md border border-warning/30 bg-warning-soft p-3">
          <p className="text-xs leading-5 text-warning">
            This keeps the active version and permanently removes {Math.max(0, model.versions.length - 1)} inactive version{model.versions.length === 2 ? "" : "s"}. Type <code className="font-mono font-semibold">{expectedPrune}</code> to continue.
          </p>
          <input
            value={pruneConfirmation}
            onChange={(event) => setPruneConfirmation(event.target.value)}
            aria-label={`Confirmation to prune versions of ${model.displayName}`}
            className={`${CONTROL_CLASS} mt-3 font-mono`}
            autoComplete="off"
          />
          <div className="mt-3 flex flex-wrap justify-end gap-2">
            <Button type="button" className="min-h-11" onClick={() => { setConfirmingPrune(false); setPruneConfirmation(""); }}>
              Keep versions
            </Button>
            <BusyButton
              type="button"
              variant="danger"
              busy={busyState[`prune:${model.assetId}`]}
              disabled={pruneConfirmation !== expectedPrune}
              onClick={() => void pruneVersions(model.assetId).then(() => { setConfirmingPrune(false); setPruneConfirmation(""); }).catch(() => {})}
            >
              <Eraser size={15} aria-hidden="true" /> Remove inactive versions
            </BusyButton>
          </div>
        </div>
      )}

      {confirming ? (
        <div className="mt-4 rounded-md border border-danger/30 bg-danger-soft p-3">
          <p className="text-xs leading-5 text-danger">
            This removes every locally stored version. Type <code className="font-mono font-semibold">{expected}</code> to continue.
          </p>
          <input
            value={confirmation}
            onChange={(event) => setConfirmation(event.target.value)}
            aria-label={`Confirmation to delete ${model.displayName}`}
            className={`${CONTROL_CLASS} mt-3 font-mono`}
            autoComplete="off"
          />
          <div className="mt-3 flex flex-wrap justify-end gap-2">
            <Button type="button" className="min-h-11" onClick={() => { setConfirming(false); setConfirmation(""); }}>
              Keep model
            </Button>
            <BusyButton
              type="button"
              variant="danger"
              busy={busyState[`delete:${model.assetId}`]}
              disabled={confirmation !== expected}
              onClick={() => void deleteModel(model.assetId).then(() => setConfirming(false)).catch(() => {})}
            >
              <Trash2 size={15} aria-hidden="true" /> Delete permanently
            </BusyButton>
          </div>
        </div>
      ) : (
        <div className="mt-4 flex flex-wrap justify-end gap-2">
          <Button type="button" className="min-h-11" onClick={findUpdates}>
            <RefreshCw size={15} aria-hidden="true" /> Find updates
          </Button>
          {model.versions.length > 1 && (
            <Button type="button" className="min-h-11" onClick={() => setConfirmingPrune(true)}>
              <Eraser size={15} aria-hidden="true" /> Clean old versions
            </Button>
          )}
          <Button type="button" variant="danger" className="min-h-11" onClick={() => setConfirming(true)}>
            <Trash2 size={15} aria-hidden="true" /> Delete
          </Button>
        </div>
      )}
    </article>
  );
}

function CapacityPlanner() {
  const hardware = useRuntimeHubStore((state) => state.hardware);
  const profile = useRuntimeHubStore((state) => state.profile);
  const runtimes = useRuntimeHubStore((state) => state.runtimes);
  const installedModels = useRuntimeHubStore((state) => state.installedModels);
  const plan = useRuntimeHubStore((state) => state.schedulingPlan);
  const planSchedule = useRuntimeHubStore((state) => state.planSchedule);
  const refreshRuntime = useRuntimeHubStore((state) => state.refreshRuntime);
  const busy = useRuntimeHubStore((state) => state.busy["schedule-plan"]);
  const error = useRuntimeHubStore((state) => state.errors["schedule-plan"]);
  const [selected, setSelected] = useState<string[]>([]);
  const schedulable = installedModels.filter((model) => model.runtime !== "mlx");

  async function preview() {
    if (!hardware || !profile || !selected.length) return;
    await Promise.all(runtimes.map((runtime) => refreshRuntime(runtime.descriptor.runtimeId).catch(() => {})));
    const current = useRuntimeHubStore.getState();
    const input = buildSchedulingInput(
      hardware,
      profile,
      current.runtimes,
      current.runtimeDetails,
      current.installedModels,
      selected,
    );
    await planSchedule(input);
  }

  return (
    <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="capacity-planner-heading">
      <SectionHeading
        title="Local capacity planner"
        description="Preview memory-aware execution waves using current RAM/VRAM, live residents, runtime slots, and each installed model’s catalog estimates."
      />
      <div id="capacity-planner-heading" className="mt-3 grid gap-2 sm:grid-cols-2">
        {schedulable.map((model) => (
          <label key={model.assetId} className="flex min-h-11 cursor-pointer items-center gap-3 rounded-md border border-border bg-surface-2 px-3 py-2">
            <input
              type="checkbox"
              checked={selected.includes(model.assetId)}
              onChange={(event) => setSelected((current) => event.target.checked ? [...current, model.assetId] : current.filter((id) => id !== model.assetId))}
              className="h-4 w-4 accent-[var(--color-accent)]"
            />
            <span className="min-w-0">
              <span className="block truncate text-sm font-medium text-foreground">{model.displayName}</span>
              <span className="block text-xs text-muted">{labelize(model.runtime)} · {formatBytes(model.estimatedRamBytes)} RAM · {formatBytes(model.estimatedVramBytes)} VRAM</span>
            </span>
          </label>
        ))}
      </div>
      {!schedulable.length && <p className="mt-3 text-sm text-muted">Install an Ollama or llama.cpp model to create a plan. MLX uses its own single-service lifecycle.</p>}
      <ErrorNotice message={error} />
      <div className="mt-3 flex justify-end">
        <BusyButton type="button" variant="primary" busy={busy} disabled={!hardware || !profile || !selected.length} onClick={() => void preview().catch(() => {})}>
          <GitCompareArrows size={15} aria-hidden="true" /> Preview execution waves
        </BusyButton>
      </div>
      {plan && (
        <div className="mt-4 flex flex-col gap-2" aria-live="polite">
          {plan.waves.map((wave) => (
            <div key={wave.wave_index} className="rounded-md border border-border bg-surface-2 p-3">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <p className="text-xs font-semibold text-foreground">Wave {wave.wave_index + 1}</p>
                <span className="text-xs text-muted">{formatBytes(wave.ram_bytes)} RAM · {formatBytes(wave.vram_bytes)} VRAM</span>
              </div>
              <div className="mt-2 flex flex-wrap gap-1.5">
                {wave.targets.map((target) => (
                  <StatusPill key={target.target_id} tone={target.queued ? "warning" : "success"}>
                    {target.model_id} · {labelize(target.residency)}{target.queued ? " · queued" : ""}
                  </StatusPill>
                ))}
              </div>
            </div>
          ))}
          {!plan.waves.length && <p className="text-sm text-muted">No targets were selected for the schedule.</p>}
        </div>
      )}
    </section>
  );
}

function InterruptedStorageCleanup() {
  const cleanup = useRuntimeHubStore((state) => state.cleanupOrphans);
  const report = useRuntimeHubStore((state) => state.cleanupReport);
  const busy = useRuntimeHubStore((state) => state.busy["cleanup-orphans"]);
  const error = useRuntimeHubStore((state) => state.errors["cleanup-orphans"]);
  const [confirmation, setConfirmation] = useState("");
  return (
    <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="runtime-cleanup-heading">
      <SectionHeading
        title="Interrupted storage cleanup"
        description="Remove only app-owned partial downloads, staging directories, and isolated trash. Unknown files and verified active versions are preserved."
      />
      <Field label="Confirmation" hint="Type CLEAN ORPHANS to authorize this bounded cleanup.">
        <input id="runtime-cleanup-heading" value={confirmation} onChange={(event) => setConfirmation(event.target.value)} className={`${CONTROL_CLASS} mt-3 font-mono`} />
      </Field>
      <ErrorNotice message={error} />
      {report && (
        <p role="status" className="mt-3 text-xs text-muted">
          Last cleanup removed {report.removedPaths} owned path{report.removedPaths === 1 ? "" : "s"} and reclaimed {formatBytes(report.reclaimedBytes)}.
        </p>
      )}
      <div className="mt-3 flex justify-end">
        <BusyButton
          type="button"
          variant="danger"
          busy={busy}
          disabled={confirmation !== "CLEAN ORPHANS"}
          onClick={() => void cleanup().then(() => setConfirmation("")).catch(() => {})}
        >
          <Eraser size={15} aria-hidden="true" /> Clean owned orphans
        </BusyButton>
      </div>
    </section>
  );
}

export function RuntimeHubModels() {
  const query = useRuntimeHubStore((state) => state.catalogQuery);
  const setQuery = useRuntimeHubStore((state) => state.setCatalogQuery);
  const results = useRuntimeHubStore((state) => state.catalogResults);
  const installedModels = useRuntimeHubStore((state) => state.installedModels);
  const searchCatalog = useRuntimeHubStore((state) => state.searchCatalog);
  const searching = useRuntimeHubStore((state) => state.busy.catalog);
  const error = useRuntimeHubStore((state) => state.errors.catalog);
  const compatibilityReport = useRuntimeHubStore((state) => state.compatibilityReport);

  function submit(event: FormEvent) {
    event.preventDefault();
    void searchCatalog().catch(() => {});
  }

  return (
    <div role="tabpanel" id="runtime-hub-panel-models" aria-labelledby="runtime-hub-tab-models" className="flex flex-col gap-6">
      <section className="flex flex-col gap-3" aria-labelledby="installed-models-heading">
        <SectionHeading
          title="Installed models"
          description="Verified artifacts are versioned, integrity checked, and kept inside app-private storage."
        />
        <div id="installed-models-heading" className="grid gap-3 lg:grid-cols-2">
          {installedModels.length ? installedModels.map((model) => <InstalledCard key={model.assetId} model={model} />) : (
            <div className="rounded-lg border border-dashed border-border p-6 text-center text-sm text-muted lg:col-span-2">
              No Runtime Hub models are installed yet. Search the catalog below to begin.
            </div>
          )}
        </div>
      </section>

      <CapacityPlanner />
      <InterruptedStorageCleanup />

      <section className="flex flex-col gap-3" aria-labelledby="model-catalog-heading">
        <SectionHeading
          title="Model catalog"
          description="Results include exact revision, license provenance, and a live fit rating for this computer."
        />
        <CompatibilityWarningBanner report={compatibilityReport} />
        <form onSubmit={submit} className="flex flex-col gap-2 sm:flex-row">
          <label className="relative min-w-0 flex-1">
            <span className="sr-only">Search model catalog</span>
            <Search size={16} className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-faint" aria-hidden="true" />
            <input
              id="model-catalog-heading"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Search by model, family, or quantization"
              className={`${CONTROL_CLASS} pl-9`}
            />
          </label>
          <BusyButton type="submit" variant="primary" busy={searching}>
            <Search size={15} aria-hidden="true" /> Search
          </BusyButton>
        </form>
        <ErrorNotice message={error} />

        {results.length > 0 ? (
          <div className="grid gap-3 lg:grid-cols-2" aria-live="polite">
            {results.map((match) => <CatalogCard key={`${match.model.sourceId}:${assetIdFor(match)}:${match.model.revision}`} match={match} />)}
          </div>
        ) : (
          <div className="rounded-lg border border-dashed border-border p-6 text-center text-sm text-muted">
            {searching ? "Searching configured catalog sources…" : "Search configured sources to compare variants and hardware fit."}
          </div>
        )}
      </section>
    </div>
  );
}
