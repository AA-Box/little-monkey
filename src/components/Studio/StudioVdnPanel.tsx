import { useCallback, useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { Play, Square, FolderOpen, RefreshCw } from "lucide-react";
import { Button } from "../ui";
import { studioClient, type GenerationEntry } from "../../lib/studioClient";

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
  const [prompt, setPrompt] = useState("");
  const [first, setFirst] = useState<string | null>(null);
  const [last, setLast] = useState<string | null>(null);
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

  async function chooseFile(set: (value: string) => void) {
    const value = await open({ multiple: false, directory: false });
    if (typeof value === "string") set(value);
  }

  async function probeRuntime() {
    if (!python || probing || running) return;
    setError(null);
    setProbInfoPlaceholder();
    setProbing(true);
    try {
      const report = await studioClient.vdnProbe(python);
      setProbeInfo(`${report.device} · compute capability ${report.computeCapability} · ${(report.vramBytes / (1024 ** 3)).toFixed(1)} GiB VRAM · PyTorch ${report.torch}`);
      setGpuFp8(report.fp8Supported);
      if (!report.fp8Supported) setFp8(false);
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

  async function run() {
    if (!python || !probeInfo || !prompt.trim() || licenseReason !== null ||
        !trustRemoteCode || running) return;
    const id = crypto.randomUUID();
    job.current = id;
    setRunning(true);
    setPhase("starting");
    setError(null);
    setCreated(null);
    try {
      const entry = await studioClient.vdnRun({
        jobId: id,
        pythonExecutable: python,
        prompt: prompt.trim(),
        firstImagePath: first,
        lastImagePath: last,
        steps,
        frames,
        seed: Number(seed) || 0,
        fp8,
        offloadDit: offload,
        acknowledgeThirdPartyCode: trustRemoteCode,
      });
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
      const cancelled = await studioClient.vdnCancel(id);
      setPhase(cancelled ? "cancelling" : "already finished");
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
            <input type="number" min={1} max={50} value={steps} disabled={running}
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
          <label className="flex items-center gap-2 text-muted">
            <input type="checkbox" checked={offload} disabled={running}
              onChange={(event) => setOffload(event.target.checked)} />
            Stream transformer blocks to GPU (24 GB VRAM)
          </label>
          <label className="flex items-center gap-2 text-muted">
            <input type="checkbox" checked={fp8} disabled={running || !gpuFp8}
              onChange={(event) => setFp8(event.target.checked)} />
            TorchAO FP8 (supported GPU only)
          </label>
        </div>

        <label className="flex items-start gap-2 text-muted">
          <input type="checkbox" checked={trustRemoteCode} disabled={running}
            onChange={(event) => setTrustRemoteCode(event.target.checked)} />
          I agree to run upstream OpenVDN Python model components
          (Hugging Face trust_remote_code=True), and have verified the source.
        </label>

        <div className="flex flex-wrap items-center gap-2">
          <Button size="sm" variant="primary"
            disabled={running || !python || !probeInfo || !prompt.trim() ||
              licenseReason !== null || !trustRemoteCode}
            onClick={() => void run()}>
            <Play size={13} /> Generate with VDN-H3
          </Button>
          {running && <Button size="sm" variant="secondary" onClick={() => void cancel()}>
            <Square size={13} /> Cancel VDN job
          </Button>}
          {phase && <span role="status" className="text-muted">Status: {phase}</span>}
        </div>
        {error && <p role="alert" className="text-danger">{error}</p>}
        {created && <p className="text-foreground">
          Video saved to Studio Gallery: {created.frameCount} frames,
          {(created.durationMs / 1000).toFixed(1)} seconds, audio/video MP4.
        </p>}
        <p className="text-muted">
          Output is verified and published into the same Studio Gallery as Wan.
          Missing CUDA dependencies, unauthorized territory, unsupported GPU, or
          incompatible OpenVDN Diffusers components are reported as errors,
          never represented as a successful render.
        </p>
      </div>
    </details>
  );
}
