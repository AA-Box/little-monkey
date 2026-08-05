import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type FrameGrid = "down_to4n_plus1" | "up_to17k_plus5";

export type GenerationTask =
  | "text_to_image"
  | "image_to_image"
  | "text_to_video"
  | "image_to_video";

export type ComponentSlot =
  | "checkpoint"
  | "diffusion_model"
  | "high_noise_diffusion_model"
  | "clip_l"
  | "clip_g"
  | "clip_vision"
  | "t5xxl"
  | "llm"
  | "vae"
  | "audio_vae"
  | "taesd";

export interface ModelComponent {
  slot: ComponentSlot;
  repo: string;
  file: string;
  sizeBytes: number;
}

export interface GenerationDefaults {
  width: number;
  height: number;
  steps: number;
  cfgScale: number;
  sampleMethod: string;
  flowShift: number | null;
  fps: number;
  videoFrames: number;
  frameGrid: FrameGrid;
}

/** A model's terms. `excludedTerritories` being non-empty is what makes the
 *  license sheet blocking rather than informational. */
export interface LicenseGate {
  id: string;
  name: string;
  url: string;
  excludedTerritories: string[];
  acceptanceRequired: boolean;
}

export interface GenerationModel {
  id: string;
  name: string;
  family: string;
  tasks: GenerationTask[];
  components: ModelComponent[];
  defaults: GenerationDefaults;
  minRamBytes: number;
  license: LicenseGate;
  extraLaunchArgs: string[];
  installed: boolean;
  missingBytes: number;
  licenseAccepted: boolean;
  fitsInMemory: boolean;
}

export interface GenerationEntry {
  entryId: string;
  artifactId: string;
  modelId: string;
  task: GenerationTask;
  prompt: string;
  negativePrompt: string;
  mediaType: string;
  sizeBytes: number;
  width: number;
  height: number;
  steps: number;
  cfgScale: number;
  seed: number;
  frameCount: number;
  fps: number;
  durationMs: number;
  createdAtMs: number;
}

export interface GenerationEngineStatus {
  supported: boolean;
  engineInstalled: boolean;
  loadedModelId: string | null;
  totalRamBytes: number;
}

export interface GenerationRequest {
  modelId: string;
  task: GenerationTask;
  prompt: string;
  negativePrompt: string;
  width: number;
  height: number;
  steps: number;
  cfgScale: number;
  seed: number;
  videoFrames: number;
  fps: number;
  initImageBase64: string | null;
}

export interface GenerationProgressPayload {
  jobId: string;
  phase: string;
  queuePosition: number;
}

export const studioClient = {
  engineStatus: () => invoke<GenerationEngineStatus>("generation_engine_status"),
  models: () => invoke<GenerationModel[]>("generation_models"),
  acceptLicense: (licenseId: string) =>
    invoke<void>("generation_accept_license", { licenseId }),
  downloadModel: (modelId: string) =>
    invoke<void>("generation_download_model", { modelId }),
  cancelDownload: (modelId: string) =>
    invoke<boolean>("generation_cancel_download", { modelId }),
  run: (request: GenerationRequest) =>
    invoke<GenerationEntry>("generation_run", { request }),
  cancel: (jobId: string) => invoke<boolean>("generation_cancel", { jobId }),
  gallery: () => invoke<GenerationEntry[]>("generation_gallery"),
  mediaDataUrl: (artifactId: string) =>
    invoke<string>("generation_media_data_url", { artifactId }),
  unloadEngine: () => invoke<void>("generation_unload_engine"),
  onProgress: (
    listener: (payload: GenerationProgressPayload) => void,
  ): Promise<UnlistenFn> =>
    listen<GenerationProgressPayload>("studio://progress", (event) =>
      listener(event.payload),
    ),
};

/** Each family snaps clip length differently — Wan rounds down onto `4n + 1`,
 *  MiniMax H3 rounds up onto `17k + 5`. The slider has to snap the same way the
 *  backend will, or the duration it shows is one the clip never has. Mirrors
 *  `normalize_video_frames` in generation.rs. */
export function normalizeVideoFrames(grid: FrameGrid, value: number): number {
  const clamped = Math.min(Math.max(Math.trunc(value), 1), 361);
  if (grid === "down_to4n_plus1") {
    if (clamped < 5) return 1;
    return Math.floor((clamped - 1) / 4) * 4 + 1;
  }
  if (clamped <= 5) return 5;
  const steps = Math.ceil((clamped - 5) / 17);
  const aligned = steps * 17 + 5;
  return aligned > 361 ? (steps - 1) * 17 + 5 : aligned;
}

/** Canvas edges must be multiples of 32, rounded up as the backend does. */
export function normalizeDimension(value: number): number {
  const clamped = Math.min(Math.max(Math.trunc(value), 32), 4096);
  return Math.min(Math.ceil(clamped / 32) * 32, 4096);
}

export function formatBytes(bytes: number): string {
  if (bytes <= 0) return "0 GB";
  const gigabytes = bytes / 1_000_000_000;
  if (gigabytes >= 1) return `${gigabytes.toFixed(1)} GB`;
  return `${Math.max(1, Math.round(bytes / 1_000_000))} MB`;
}

export function isVideoTask(task: GenerationTask): boolean {
  return task === "text_to_video" || task === "image_to_video";
}

export function needsInitImage(task: GenerationTask): boolean {
  return task === "image_to_image" || task === "image_to_video";
}
