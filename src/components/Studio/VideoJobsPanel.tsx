import { useEffect, useState } from "react";
import { Button } from "../ui";
import { studioClient, type GenerationEntry, type GenerationModel, type GenerationRequest, type VideoJob } from "../../lib/studioClient";
import { useStudioRunStore } from "../../store/studioRunStore";
import { VideoTimelinePanel } from "./VideoTimelinePanel";

export function VideoJobsPanel() {
  const [jobs, setJobs] = useState<VideoJob[]>([]);
  const [galleryVideos, setGalleryVideos] = useState<GenerationEntry[]>([]);
  const [galleryImages, setGalleryImages] = useState<GenerationEntry[]>([]);
  const [targetImageId, setTargetImageId] = useState("");
  const [nativeModels, setNativeModels] = useState<GenerationModel[]>([]);
  const [sourceGalleryId, setSourceGalleryId] = useState("");
  const [nativeModelId, setNativeModelId] = useState("");
  const [galleryPrompt, setGalleryPrompt] = useState("");
  const [nextPrompts, setNextPrompts] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  useEffect(() => {
    let mounted = true;
    let initial = true;
    let previous = new Set<string>();
    const refresh = async () => {
      try {
        const next = await studioClient.videoJobs();
        if (!mounted) return;
        const completed = new Set(next.filter((job) => job.status === "completed").map((job) => job.id));
        if (!initial && [...completed].some((id) => !previous.has(id))) {
          useStudioRunStore.setState((state) => ({ completions: state.completions + 1 }));
        }
        initial = false;
        previous = completed;
        setJobs(next);
      } catch (cause) {
        if (mounted) setError(String(cause));
      }
    };
    void refresh();
    const interval = setInterval(() => void refresh(), 2000);
    return () => { mounted = false; clearInterval(interval); };
  }, []);

  // Native continuation requires no ComfyUI graph. Gallery clips are resolved
  // and decoded in Rust; the browser never receives a full MP4 as base64.
  const refreshSources = async () => {
    const [gallery, models] = await Promise.all([studioClient.gallery(), studioClient.models()]);
    setGalleryVideos(gallery.filter((entry) => entry.mediaType === "video/mp4" || entry.mediaType === "video/webm"));
    setGalleryImages(gallery.filter((entry) => entry.mediaType === "image/png" || entry.mediaType === "image/jpeg"));
    setNativeModels(models.filter((model) =>
      model.installed && model.licenseAccepted && model.tasks.includes("image_to_video")
    ));
  };

  useEffect(() => {
    void refreshSources().catch((cause) => setError(String(cause)));
  }, []);

  const nativeModel = nativeModels.find((model) => model.id === nativeModelId) ?? nativeModels[0];
  const selectedGalleryId = sourceGalleryId || galleryVideos[0]?.entryId || "";

  const continueGallery = async () => {
    const prompt = galleryPrompt.trim();
    if (!prompt || !nativeModel || !selectedGalleryId) {
      setError("Choose a published clip, a local image-to-video model, and a next-scene prompt.");
      return;
    }
    if (targetImageId && nativeModel.engine !== "stable_diffusion_cpp") {
      setError("First+last-frame conditioning requires a compatible local sd-server video model.");
      return;
    }
    const defaults = nativeModel.defaults;
    const request: GenerationRequest = {
      modelId: nativeModel.id, task: "image_to_video", prompt,
      negativePrompt: "", width: defaults.width, height: defaults.height,
      steps: defaults.steps, cfgScale: defaults.cfgScale,
      sampleMethod: defaults.sampleMethod, scheduler: "", clipSkip: -1,
      eta: null, strength: 0.75, hires: null, seed: -1, batchCount: 1,
      videoFrames: defaults.videoFrames, fps: defaults.fps,
      speakerFile: null, language: null, initImageBase64: null,
      maskImageBase64: null, adPrompt: null, adNegativePrompt: null,
      controlImageBase64: null, controlStrength: null,
      ipAdapterImageBase64: null, ipAdapterStrength: null,
      refImagesBase64: [], increaseRefIndex: false, loras: [],
      componentOverrides: [],
    };
    setBusy("gallery-continuation");
    setError(null);
    try {
      await studioClient.videoSubmit(request, crypto.randomUUID(), [], null, selectedGalleryId, targetImageId || null);
      setJobs(await studioClient.videoJobs());
      setGalleryPrompt("");
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(null);
    }
  };

  const act = async (job: VideoJob, action: "resume" | "cancel") => {
    setBusy(job.id);
    setError(null);
    try {
      if (action === "resume") await studioClient.videoResume(job.id);
      else await studioClient.videoCancel(job.id);
      setJobs(await studioClient.videoJobs());
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(null);
    }
  };

  const continueFrom = async (job: VideoJob) => {
    const prompt = (nextPrompts[job.id] ?? job.prompt).trim();
    if (!prompt) {
      setError("Describe the next scene");
      return;
    }
    setBusy(job.id);
    setError(null);
    try {
      await studioClient.videoContinue(job.id, prompt, crypto.randomUUID());
      setJobs(await studioClient.videoJobs());
      setNextPrompts((current) => ({ ...current, [job.id]: "" }));
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(null);
    }
  };

  // A fresh installation may have gallery clips but no native video jobs yet.
  // The native continuation form and the timeline must remain reachable.
  return (
    <section className="mx-1 rounded-lg border border-border bg-surface p-3" aria-label="Durable video jobs">
      <h3 className="mb-1 text-xs font-semibold">Video jobs</h3>
      <p className="mb-2 text-[11px] text-muted">
        Queued jobs recover automatically when the app starts. Continue from a clip to feed its
        decoded final frame into the next video. FFmpeg is required. Interrupted renders restart
        from their saved seed; there is no mid-frame checkpoint.
      </p>
      {error && <p role="alert" className="mb-2 text-xs text-danger">{error}</p>}
      <div className="mb-3 grid gap-2 rounded border border-border bg-background p-2">
        <div className="flex items-center justify-between gap-2">
          <span className="text-xs font-medium">Native continuation from Studio gallery</span>
          <Button size="sm" variant="secondary" disabled={busy !== null}
            onClick={() => void refreshSources().catch((cause) => setError(String(cause)))}>
            Refresh sources
          </Button>
        </div>
        <p className="text-[11px] text-muted">
          Pick an existing MP4/WebM or exported timeline. Little Monkey decodes the final frame
          locally and uses it as the next clip's first image. An optional published ending-frame
          image is sent to the compatible native model as end_image. No external ComfyUI service
          is used. Motion latents and audio conditioning are not carried between runs.
        </p>
        <label className="grid gap-1 text-[11px]">
          Source video
          <select aria-label="Source gallery video" className="rounded border border-border bg-surface px-2 py-1 text-xs"
            value={selectedGalleryId} onChange={(event) => setSourceGalleryId(event.target.value)}>
            {galleryVideos.length === 0 && <option value="">No published clips</option>}
            {galleryVideos.map((entry) => <option value={entry.entryId} key={entry.entryId}>
              {entry.prompt.slice(0, 60) || entry.modelId} · {new Date(entry.createdAtMs).toLocaleString()}
            </option>)}
          </select>
        </label>
        <label className="grid gap-1 text-[11px]">
          Local image-to-video model
          <select aria-label="Native video model" className="rounded border border-border bg-surface px-2 py-1 text-xs"
            value={nativeModel?.id ?? ""} onChange={(event) => setNativeModelId(event.target.value)}>
            {nativeModels.length === 0 && <option value="">Install an image-to-video model in Studio</option>}
            {nativeModels.map((model) => <option value={model.id} key={model.id}>{model.name}</option>)}
          </select>
        </label>
        <label className="grid gap-1 text-[11px]">
          Target ending frame (optional, compatible sd-server models)
          <select aria-label="Target gallery ending frame" className="rounded border border-border bg-surface px-2 py-1 text-xs"
            value={targetImageId} onChange={(event) => setTargetImageId(event.target.value)}>
            <option value="">No ending frame constraint</option>
            {galleryImages.map((entry) => <option value={entry.entryId} key={entry.entryId}>
              {entry.prompt.slice(0, 60) || entry.modelId} · {new Date(entry.createdAtMs).toLocaleString()}
            </option>)}
          </select>
        </label>
        <input aria-label="Gallery continuation prompt" placeholder="Describe the next scene"
          className="rounded border border-border bg-surface px-2 py-1 text-xs"
          value={galleryPrompt} onChange={(event) => setGalleryPrompt(event.target.value)}/>
        <Button size="sm" variant="secondary" disabled={busy !== null || !selectedGalleryId || !nativeModel || !galleryPrompt.trim() || Boolean(targetImageId && nativeModel.engine !== "stable_diffusion_cpp")}
          onClick={() => void continueGallery()}>Generate next scene locally</Button>
      </div>
      <div className="max-h-32 space-y-2 overflow-y-auto">
        {jobs.slice(0, 24).map((job) => (
          <div key={job.id} className="flex items-center gap-2 text-xs">
            <span className="min-w-0 flex-1 truncate" title={job.prompt}>{job.prompt || job.modelId}</span>
            <span className="shrink-0 font-mono text-[11px] text-muted">
              {job.status} · seed {job.seed}{job.status === "running" ? " · rendering" : ""}
            </span>
            {(job.sourceJobId || job.sourceGalleryEntryId) && <span className="text-[11px] text-muted" title={job.sourceGalleryEntryId ?? job.sourceJobId ?? ""}>↳ clip</span>}
            {job.error && <span className="max-w-48 truncate text-[11px] text-danger" title={job.error}>
              {job.error}
            </span>}
            {(job.status === "interrupted" || job.status === "failed") && (
              <Button size="sm" variant="secondary" disabled={busy === job.id}
                onClick={() => void act(job, "resume")}>Resume</Button>
            )}
            {(job.status === "queued" || job.status === "running") && (
              <Button size="sm" variant="secondary" disabled={busy === job.id}
                onClick={() => void act(job, "cancel")}>Cancel</Button>
            )}
            {(["queued", "running", "completed"] as const).some((status) => status === job.status) && (
              <span className="flex min-w-0 items-center gap-1">
                <input aria-label="Next scene prompt" placeholder="Next scene prompt"
                  className="w-36 rounded border border-border bg-background px-2 py-1 text-[11px]"
                  value={nextPrompts[job.id] ?? ""}
                  onChange={(event) => setNextPrompts((state) => ({ ...state, [job.id]: event.target.value }))}
                />
                <Button size="sm" variant="secondary" disabled={busy === job.id}
                  onClick={() => void continueFrom(job)}>Continue</Button>
              </span>
            )}
          </div>
        ))}
      </div>
      <VideoTimelinePanel jobs={jobs} />
    </section>
  );
}
