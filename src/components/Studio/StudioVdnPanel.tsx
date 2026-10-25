import { useCallback, useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { Play, Square, FolderOpen, RefreshCw } from "lucide-react";
import { Button } from "../ui";
import { studioClient, type GenerationEntry, type VdnBackend, type DurableVdnJob } from "../../lib/studioClient";

type PhaseEvent = { jobId: string; phase: string; detail: string };
type Region = "eu" | "uk" | "us" | "kr" | "other" | null;

/** Executes OpenVDN's actual hybrid-attention model through an app-owned
 * standalone Python worker. No FreeVideo process or ComfyUI server.
 * The Rust backend, never UI state, controls local model licensing. */
export function StudioVdnPanel({
  onCompleted,
  onOpenModels,
}: {
  onCompleted: () => void | Promise<void>;
  onOpenModels?: () => void;
}) {
  const [python, setPython] = useState("");
  const [backend, setBackend] = useState<VdnBackend>("diffusers");
  const [repoRoot, setRepoRoot] = useState("");
  const [checkpointRoot, setCheckpointRoot] = useState("");
  const [gpuClass, setGpuClass] = useState<"h200" | "b200">("h200");
  const [serverPort, setServerPort] = useState(30010);
  const [prompt, setPrompt] = useState("");
  const [first, setFirst] = useState<string | null>(null);
  const [last, setLast] = useState<string | null>(null);
  const [sourceGalleryId, setSourceGalleryId] = useState<string | null>(null);
  const [availableClips, setAvailableClips] = useState<GenerationEntry[]>([]);
  const [durableJobs, setDurableJobs] = useState<DurableVdnJob[]>([]);
  const [references, setReferences] = useState<string[]>([]);
  const [referenceVideo, setReferenceVideo] = useState<string | null>(null);
  const [referenceAudio, setReferenceAudio] = useState<string | null>(null);
  const [loraPaths, setLoraPaths] = useState<string[]>([]);
  const [steps, setSteps] = useState(8);
  const [frames, setFrames] = useState(141);
  const [seed, setSeed] = useState("0");
  const [fp8, setFp8] = useState(false);
  const [offload, setOffload] = useState(true);
  const [trustRemoteCode, setTrustRemoteCode] = useState(false);
  const [licenseReason, setLicenseReason] = useState<string | null | undefined>(undefined);
  const [region, setRegion] = useState<Region>(null);
  const [running, setRunning] = useState(false);
  const [phase, setPhase] = useState("");
  const [probeInfo, setProbeInfo] = useState<string | null>(null);
  const [probing, setProbing] = useState(false);
  const [gpuFp8, setGpuFp8] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [created, setCreated] = useState<GenerationEntry | null>(null);
  const [preview, setPreview] = useState<GenerationEntry | null>(null);
  const job = useRef<string | null>(null);

  const refreshLicense = useCallback(async () => {
    try {
      const [reason, usageRegion] = await Promise.all([
        studioClient.vdnLicenseStatus(),
        studioClient.licenseRegion(),
      ]);
      setLicenseReason(reason);
      setRegion(usageRegion);
    } catch (cause) {
      setLicenseReason(String(cause));
    }
  }, []);

  useEffect(() => {
    void refreshLicense();
    void studioClient.vdnJobs().then(setDurableJobs).catch(() => {});
    void studioClient.gallery().then(items => setAvailableClips(items.filter(item => item.mediaType === "video/mp4" || item.mediaType === "video/webm"))).catch(() => {});
    let active = true;
    let unlisten: (() => void) | undefined;
    void listen<PhaseEvent>("studio://vdn-progress", (event) => {
      if (active && job.current === event.payload.jobId) {
        setPhase(event.payload.phase);
      }
    }).then((clean) => {
      if (active) unlisten = clean;
      else clean();
    }).catch(() => {});
    return () => { active = false; unlisten?.(); };
  }, [refreshLicense]);

  async function chooseFile(set: (value: string) => void, directory = false) {
    const value = await open({ multiple: false, directory });
    if (typeof value === "string") set(value);
  }

  async function probeRuntime() {
    if (!python || probing || running) return;
    setError(null);
    setProbInfoPlaceholder();
    setProbing(true);
    try {
      const report = await studioClient.vdnProbe(
        python, backend, repoRoot || null, checkpointRoot || null,
        gpuClass, serverPort,
      );
      if (backend === "free_video_adaptive") {
        setProbeInfo("Installed FreeVideo adaptive runtime detected; actual model readiness is checked at execution.");
        setGpuFp8(false);
      } else if (backend === "sglang") {
        setProbeInfo(`Serving ${report.model} through 127.0.0.1:${report.port ?? serverPort}`);
        setGpuFp8(false);
        setFp8(false);
      } else {
        setProbeInfo(`${report.device} · ${report.computeCapability} · ${((report.vramBytes ?? 0) / (1024 ** 3)).toFixed(1)} GiB VRAM · ${report.visibleGpuCount ?? 1} CUDA GPUs · PyTorch ${report.torch}`);
        setGpuFp8(report.fp8Supported === true);
        if (!report.fp8Supported) setFp8(false);
      }
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setProbeInfo(null);
    } finally {
      setProbing(false);
    }
  }

  function setProbInfoPlaceholder() {
    setProbeInfo(null);
  }

  async function acceptCommunityTerms() {
    try {
      await studioClient.vdnAcceptTerms();
      await refreshLicense();
    } catch (cause) {
      setError(String(cause));
    }
  }

  async function queuedRun(request: Parameters<typeof studioClient.vdnSubmit>[0]): Promise<GenerationEntry> {
    const record = await studioClient.vdnSubmit(request);
    setDurableJobs((records) => [...records, record]);
    for (;;) {
      await new Promise<void>((resolve) => setTimeout(resolve, 1000));
      const records = await studioClient.vdnJobs();
      setDurableJobs(records);
      const current = records.find((item) => item.id === request.jobId);
      if (!current) throw new Error("Persisted VDN job disappeared");
      setPhase(current.status);
      if (current.status === "completed") {
        const gallery = await studioClient.gallery();
        const result = gallery.find((entry) => entry.entryId === current.resultEntryId);
        if (!result) throw new Error("VDN completed without a Gallery artifact");
        return result;
      }
      if (["cancelled", "failed", "interrupted"].includes(current.status)) {
        throw new Error(current.error || `VDN job ${current.status}`);
      }
    }
  }

  async function run(twoPass = false) {
    if (!python || !probeInfo || !prompt.trim() || licenseReason !== null ||
        !trustRemoteCode || running) return;
    const id = crypto.randomUUID();
    job.current = id;
    setRunning(true);
    setPhase("queued");
    setError(null);
    setCreated(null);
    setPreview(null);
    try {
      const request = {
        jobId: id, pythonExecutable: python, prompt: prompt.trim(),
        firstImagePath: first, lastImagePath: last,
        sourceGalleryEntryId: sourceGalleryId,
        referenceImagePaths: references,
        referenceVideoPath: referenceVideo,
        referenceAudioPath: referenceAudio,
        loraPaths, steps, frames, seed: Number(seed) || 0,
        fp8, offloadDit: offload, backend,
        repoRoot: repoRoot || null,
        checkpointRoot: checkpointRoot || null,
        gpuClass: backend === "optimized_ulysses" ? gpuClass : null,
        serverPort: backend === "sglang" ? serverPort : null,
        acknowledgeThirdPartyCode: trustRemoteCode,
      };
      if (twoPass && frames > 124) {
        setPhase("preview");
        const previewEntry = await queuedRun({ ...request, frames: 124 });
        setPreview(previewEntry);
        await onCompleted();
        request.jobId = crypto.randomUUID();
        job.current = request.jobId;
      }
      const entry = await queuedRun(request);
      setCreated(entry);
      setPhase("completed");
      await onCompleted();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setPhase("failed");
    } finally {
      setRunning(false);
      job.current = null;
      void refreshLicense();
    }
  }

  async function cancel() {
    const id = job.current;
    if (!id) return;
    try {
      await studioClient.vdnCancelJob(id);
      setPhase("cancelling");
    } catch (cause) {
      setError(String(cause));
    }
  }

  return (
    <details className="mb-3 shrink-0 rounded-lg border border-border bg-background/60 p-3 text-xs">
      <summary className="cursor-pointer font-semibold text-foreground">
        Video DeltaNet / VDN-H3 — independent hybrid-attention engine
      </summary>
      <div className="mt-3 grid gap-3">
        <p className="text-muted">
          Run the genuine OpenVDN Diffusers hybrid-attention pipeline with its
          trained 8-step VDN-H3 weights, directly from Little Monkey.
          It requires an installed compatible CUDA Python environment and
          substantial disk/RAM. No FreeVideo or ComfyUI dependency.
          On macOS, use the native Wan engine above/below instead.
        </p>

        <div className="rounded border border-border bg-background p-2">
          {licenseReason === undefined ? (
            <p className="text-muted">Checking locally stored model rights…</p>
          ) : licenseReason ? (
            <>
              <p role="alert" className="text-warning">{licenseReason}</p>
              <div className="mt-2 flex flex-wrap gap-2">
                {region === "other" && (
                  <Button size="sm" variant="secondary"
                    onClick={() => void acceptCommunityTerms()}>
                    Accept upstream MiniMax H3 community terms
                  </Button>
                )}
                {onOpenModels && (
                  <Button size="sm" variant="secondary" onClick={onOpenModels}>
                    Manage jurisdiction / enterprise license
                  </Button>
                )}
                <Button size="sm" variant="secondary" onClick={() => void refreshLicense()}>
                  <RefreshCw size={13} /> Recheck
                </Button>
              </div>
            </>
          ) : <p className="text-muted">VDN model license is eligible for the declared territory.</p>}
        </div>

        <label className="grid gap-1 text-muted">
          VDN execution backend
          <select aria-label="VDN execution backend" disabled={running}
            className="rounded border border-border bg-background px-2 py-1 text-foreground"
            value={backend} onChange={(event) => {
              const next = event.target.value as VdnBackend;
              setBackend(next);
              setProbeInfo(null);
              if (next !== "diffusers") setSteps(8);
            }}>
            <option value="diffusers">Portable Diffusers — single CUDA GPU with offload</option>
            <option value="optimized_single">OpenVDN tuned kernels — Linux, one large CUDA GPU</option>
            <option value="optimized_ulysses">OpenVDN Ulysses — Linux, 8 H200/B200 GPUs</option>
            <option value="sglang">SGLang Diffusion — local accelerated server (1/2/4/8 GPUs)</option>
            <option value="free_video_adaptive">Adaptive FreeVideo — multimodal references and low-memory runtime</option>
          </select>
        </label>

        {backend === "free_video_adaptive" && <label className="grid gap-1 text-muted">
          Prepared FreeVideo model cache
          <div className="flex gap-2">
            <input className="min-w-0 flex-1 rounded border border-border bg-background p-1 text-foreground"
              value={checkpointRoot} disabled={running}
              onChange={(event) => { setCheckpointRoot(event.target.value); setProbeInfo(null); }} />
            <Button size="sm" variant="secondary" disabled={running}
              onClick={() => void chooseFile((value) => { setCheckpointRoot(value); setProbeInfo(null); }, true)}>
              Choose cache
            </Button>
          </div>
        </label>}
        {(backend === "optimized_single" || backend === "optimized_ulysses") && (
          <div className="grid gap-2">
            <label className="grid gap-1 text-muted">
              Official OpenVDN Git checkout
              <div className="flex flex-wrap gap-2">
                <input className="min-w-0 flex-1 rounded border border-border bg-background px-2 py-1 font-mono text-foreground"
                  value={repoRoot} disabled={running} onChange={(event) => {
                    setRepoRoot(event.target.value); setProbeInfo(null);
                  }} placeholder="/home/user/vdn-minimax-h3" />
                <Button size="sm" variant="secondary" disabled={running}
                  onClick={() => void chooseFile((v) => {
                    setRepoRoot(v); setProbeInfo(null);
                  }, true)}>Choose folder</Button>
              </div>
            </label>
            <label className="grid gap-1 text-muted">
              Existing authorized OpenVDN 8-step checkpoint directory (h3-base + stage-dmd-step-250)
              <div className="flex flex-wrap gap-2">
                <input className="min-w-0 flex-1 rounded border border-border bg-background px-2 py-1 font-mono text-foreground"
                  value={checkpointRoot} disabled={running} onChange={(event) => {
                    setCheckpointRoot(event.target.value); setProbeInfo(null);
                  }} placeholder="/data/ckpts" />
                <Button size="sm" variant="secondary" disabled={running}
                  onClick={() => void chooseFile((v) => {
                    setCheckpointRoot(v); setProbeInfo(null);
                  }, true)}>Choose folder</Button>
              </div>
            </label>
            {backend === "optimized_ulysses" && (
              <label className="grid gap-1 text-muted">
                GPU configuration (8 matching GPU devices required)
                <select value={gpuClass} disabled={running}
                  className="rounded border border-border bg-background px-2 py-1 text-foreground"
                  onChange={(event) => { setGpuClass(event.target.value as "h200" | "b200"); setProbeInfo(null); }}>
                  <option value="h200">8 × H200</option>
                  <option value="b200">8 × B200</option>
                </select>
              </label>
            )}
          </div>
        )}
        {backend === "sglang" && (
          <label className="grid gap-1 text-muted">
            Existing OpenVDN SGLang server port (loopback only)
            <input type="number" min={1} max={65535}
              className="rounded border border-border bg-background px-2 py-1 text-foreground"
              value={serverPort} disabled={running}
              onChange={(event) => { setServerPort(Number(event.target.value)); setProbeInfo(null); }}/>
            <span>Server must serve OpenVDN/vdn-minimax-h3.
              Stopping a client job may not stop GPU work inside existing SGLang releases.</span>
          </label>
        )}

        <label className="grid gap-1 text-muted">
          Python interpreter from the installed OpenVDN-compatible environment
          <div className="flex items-center gap-2">
            <input className="min-w-0 flex-1 rounded border border-border bg-background px-2 py-1 font-mono text-foreground"
              placeholder="/path/to/vdn-venv/bin/python"
              value={python} onChange={(event) => { setPython(event.target.value); setProbeInfo(null); }}
              disabled={running} />
            <Button size="sm" variant="secondary" disabled={running}
              onClick={() => void chooseFile((value) => { setPython(value); setProbeInfo(null); })}>
              <FolderOpen size={13} /> Browse</Button>
          </div>
        </label>

        <div className="flex flex-wrap items-center gap-2">
          <Button size="sm" variant="secondary" disabled={!python || running || probing}
            onClick={() => void probeRuntime()}>
            <RefreshCw size={13} /> {probing ? "Checking GPU…" : "Check installed VDN runtime"}
          </Button>
          {probeInfo && <span role="status" className="text-muted">{probeInfo}</span>}
        </div>

        <label className="grid gap-1 text-muted">
          Prompt
          <textarea rows={3} className="rounded border border-border bg-background p-2 text-foreground"
            placeholder="Describe the video…"
            value={prompt} onChange={(event) => setPrompt(event.target.value)}
            disabled={running} />
        </label>

        <div className="grid gap-2 sm:grid-cols-2">
          {([["First frame", first, setFirst], ["Last frame", last, setLast]] as const).map(
            ([label, path, set]) => (
              <div className="grid gap-1" key={label}>
                <span className="text-muted">{label} (PNG/JPEG, optional)</span>
                <div className="flex flex-wrap gap-2">
                  <Button size="sm" variant="secondary" disabled={running}
                    onClick={() => void chooseFile(set)}>
                    <FolderOpen size={13} /> {path ? "Replace image" : "Choose image"}
                  </Button>
                  {path && <Button size="sm" variant="secondary" disabled={running}
                    onClick={() => set(null)}>Clear</Button>}
                </div>
                {path && <span className="truncate font-mono text-muted">{path}</span>}
              </div>
            ),
          )}
        </div>

        <div className="grid gap-2 sm:grid-cols-3">
          <label className="grid gap-1 text-muted">Model evaluations
            <input type="number" min={1} max={50} value={steps}
              disabled={running || backend !== "diffusers"}
              className="rounded border border-border bg-background p-1 text-foreground"
              onChange={(event) => setSteps(Math.max(1, Math.min(50, Number(event.target.value) || 8)))} />
          </label>
          <label className="grid gap-1 text-muted">Duration (24 fps)
            <select className="rounded border border-border bg-background p-1 text-foreground"
              value={frames} disabled={running}
              onChange={(event) => setFrames(Number(event.target.value))}>
              {[124, 141, 175, 209, 260, 345].map((value) =>
                <option value={value} key={value}>{(value / 24).toFixed(1)} s — {value} frames</option>
              )}
            </select>
          </label>
          <label className="grid gap-1 text-muted">Seed
            <input type="number" min={0} value={seed} disabled={running}
              className="rounded border border-border bg-background p-1 text-foreground"
              onChange={(event) => setSeed(event.target.value)} />
          </label>
        </div>

        <div className="flex flex-wrap gap-4">
          {backend === "diffusers" && <label className="flex items-center gap-2 text-muted">
            <input type="checkbox" checked={offload} disabled={running}
              onChange={(event) => setOffload(event.target.checked)} />
            Stream transformer blocks to GPU (24 GB VRAM)
          </label>}
          {(backend === "diffusers" || backend === "optimized_single") && <label className="flex items-center gap-2 text-muted">
            <input type="checkbox" checked={fp8} disabled={running || !gpuFp8}
              onChange={(event) => setFp8(event.target.checked)} />
            FP8 (supported GPUs only)
          </label>}
        </div>

        {(backend === "optimized_single" || backend === "optimized_ulysses" || backend === "free_video_adaptive") &&
          <div className="grid gap-1 text-muted">
            <span>Reference images/video (native OpenVDN Ref2VA, visual frame conditioning)</span>
            <div className="flex items-center gap-2">
              <Button size="sm" variant="secondary" disabled={running || references.length >= 8}
                onClick={() => void open({ multiple: true, filters: [{ name: "Images", extensions: ["png", "jpg", "jpeg"] }] })
                  .then((selected) => {
                    const paths = typeof selected === "string" ? [selected] : (selected ?? []);
                    setReferences((current) => [...new Set([...current, ...paths])].slice(0, 8));
                  })}>
                Add reference images
              </Button>
              <Button size="sm" variant="secondary" disabled={running || references.length === 0}
                onClick={() => setReferences([])}>Clear</Button>
              <span>{references.length}/8 images</span>
              <Button size="sm" variant="secondary" disabled={running}
                onClick={() => void chooseFile(setReferenceVideo)}>Choose video</Button>
              {referenceVideo && <Button size="sm" variant="secondary" disabled={running}
                onClick={() => setReferenceVideo(null)}>Remove video</Button>}
              {backend === "free_video_adaptive" && <>
                <Button size="sm" variant="secondary" disabled={running}
                  onClick={() => void chooseFile(setReferenceAudio)}>Choose audio reference</Button>
                {referenceAudio && <Button size="sm" variant="secondary" disabled={running}
                  onClick={() => setReferenceAudio(null)}>Remove audio</Button>}
              </>}
            </div>
          </div>}
        {(backend === "optimized_single" || backend === "optimized_ulysses") &&
          <div className="grid gap-1 text-muted">
            <span>External LoRA adapters (up to 4 safetensors)</span>
            <div className="flex items-center gap-2">
              <Button size="sm" variant="secondary" disabled={running || loraPaths.length >= 4}
                onClick={() => void open({ multiple: true, filters: [{ name: "LoRA", extensions: ["safetensors"] }] })
                  .then((selected) => {
                    const paths = typeof selected === "string" ? [selected] : (selected ?? []);
                    setLoraPaths((current) => [...new Set([...current, ...paths])].slice(0, 4));
                  })}>Add LoRA</Button>
              <Button size="sm" variant="secondary" disabled={running || loraPaths.length === 0}
                onClick={() => setLoraPaths([])}>Clear</Button>
              <span>{loraPaths.length}/4</span>
            </div>
          </div>}
        <label className="grid gap-1 text-muted">
          Continue from a previous Gallery clip (true final frame)
          <select className="rounded border border-border bg-background p-1 text-foreground"
            value={sourceGalleryId ?? ""} disabled={running || Boolean(first)}
            onChange={(event) => setSourceGalleryId(event.target.value || null)}>
            <option value="">No previous clip</option>
            {availableClips.map((entry) =>
              <option key={entry.entryId} value={entry.entryId}>
                {entry.prompt.slice(0, 48)} · {entry.entryId.slice(-8)}
              </option>
            )}
          </select>
        </label>
        <label className="flex items-start gap-2 text-muted">
          <input type="checkbox" checked={trustRemoteCode} disabled={running}
            onChange={(event) => setTrustRemoteCode(event.target.checked)} />
          I have verified and authorize running the selected official OpenVDN
          source/runtime and the upstream model components, which may execute Python code.
        </label>

        <div className="flex flex-wrap items-center gap-2">
          <Button size="sm" variant="primary"
            disabled={running || !python || !probeInfo || !prompt.trim() ||
              licenseReason !== null || !trustRemoteCode}
            onClick={() => void run()}>
            <Play size={13} /> Generate with VDN-H3
          </Button>
          <Button size="sm" variant="secondary"
            disabled={running || frames <= 124 || !python || !probeInfo || !prompt.trim() ||
              licenseReason !== null || !trustRemoteCode}
            onClick={() => void run(true)}>
            <Play size={13} /> Preview then render (two passes)
          </Button>
          {running && <Button size="sm" variant="secondary" onClick={() => void cancel()}>
            <Square size={13} /> Cancel VDN job
          </Button>}
          {phase && <span role="status" className="text-muted">Status: {phase}</span>}
        </div>
        {error && <p role="alert" className="text-danger">{error}</p>}
        {preview && <p className="text-foreground">
          Preview saved to Gallery: {preview.frameCount} frames. This uses a separate real inference pass, not reused motion latents.
        </p>}
        {created && <p className="text-foreground">
          Video saved to Studio Gallery: {created.frameCount} frames,
          {(created.durationMs / 1000).toFixed(1)} seconds, audio/video MP4.
        </p>}
        <p className="text-muted">
          Output is verified and published into the same Studio Gallery as Wan.
          Missing CUDA dependencies, unauthorized territory, unsupported GPU, or
          incompatible upstream OpenVDN or SGLang components are reported as errors,
          never represented as a successful render.
        </p>
      </div>
    </details>
  );
}
