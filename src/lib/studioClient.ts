import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type FrameGrid = "down_to4n_plus1" | "up_to17k_plus5";

export type GenerationTask =
  | "text_to_image"
  | "image_to_image"
  | "text_to_video"
  | "image_to_video"
  /** Served by llama-tts, not sd-server. */
  | "text_to_speech";

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
  | "taesd"
  /** Speech only, and served by llama-tts rather than sd-server. */
  | "mmproj"
  | "vocoder";

/** Where a component's bytes come from. A file the user already has is a
 *  first-class source, referenced where it lies and never fetched. */
export type ComponentSource =
  | { kind: "hugging_face"; repo: string; file: string }
  | { kind: "local_file"; path: string };

export interface ModelComponent {
  slot: ComponentSlot;
  source: ComponentSource;
  sizeBytes: number;
}

/** The flat file name a component takes on disk. */
export function componentFileName(component: ModelComponent): string {
  const raw =
    component.source.kind === "hugging_face"
      ? component.source.file
      : component.source.path;
  const parts = raw.split(/[/\\]/);
  return parts[parts.length - 1] ?? raw;
}

/** One LoRA applied to a generation. Any model takes any number of them. */
export interface LoraSelection {
  path: string;
  multiplier: number;
  isHighNoise: boolean;
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

/** A model exactly as stored in the user's library. */
export interface GenerationModelSpec {
  id: string;
  name: string;
  family: string;
  tasks: GenerationTask[];
  components: ModelComponent[];
  defaults: GenerationDefaults;
  minRamBytes: number;
  license: LicenseGate;
  extraLaunchArgs: string[];
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

/** The engine's high-resolution fix: sample, upscale, denoise again. */
export interface HiresSettings {
  scale: number;
  /** 0 reuses the first pass's step count. */
  steps: number;
  denoisingStrength: number;
  upscaler: string;
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
  /** Empty falls back to the model's own default. */
  sampleMethod: string;
  /** Empty leaves the engine's own sigma schedule. */
  scheduler: string;
  /** -1 is whatever the model was trained with. */
  clipSkip: number;
  eta: number | null;
  /** How far an init image is redrawn. Only used by the image-driven tasks. */
  strength: number | null;
  /** The second, higher-resolution pass. Null disables it. */
  hires: HiresSettings | null;
  seed: number;
  videoFrames: number;
  fps: number;
  /** Speech only: a reference clip whose voice the utterance is spoken in. */
  speakerFile: string | null;
  /** Speech only: ISO 639-1 code. Null leaves the model's own default. */
  language: string | null;
  initImageBase64: string | null;
  loras: LoraSelection[];
}

export interface GenerationProgressPayload {
  jobId: string;
  phase: string;
  queuePosition: number;
  /** Absent while the engine is still loading weights, which it does not
   *  count towards the sampling pass. */
  percent: number | null;
  step: number | null;
  totalSteps: number | null;
}

export const studioClient = {
  engineStatus: () => invoke<GenerationEngineStatus>("generation_engine_status"),
  models: () => invoke<GenerationModel[]>("generation_models"),
  addModel: (spec: GenerationModelSpec) =>
    invoke<GenerationModelSpec[]>("generation_add_model", { spec }),
  removeModel: (modelId: string) =>
    invoke<void>("generation_remove_model", { modelId }),
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

/** Speech runs on llama-tts, which has no canvas and no sampler — the whole
 *  diffusion control set is meaningless for it. */
export function isSpeechTask(task: GenerationTask): boolean {
  return task === "text_to_speech";
}

/** The task that takes an existing asset back as its starting point, so a
 *  generated image can be edited by generating from it. */
export function editTaskFor(model: GenerationModel): GenerationTask | null {
  return (
    (["image_to_image", "image_to_video"] as GenerationTask[]).find((task) =>
      model.tasks.includes(task),
    ) ?? null
  );
}

/** Every slot the engine exposes, with the flag it maps to. Shown verbatim in
 *  the picker because the flag is the least ambiguous label there is — the app
 *  never guesses which slot a file fills. */
export const COMPONENT_SLOTS: { slot: ComponentSlot; flag: string }[] = [
  { slot: "checkpoint", flag: "--model" },
  { slot: "diffusion_model", flag: "--diffusion-model" },
  { slot: "high_noise_diffusion_model", flag: "--high-noise-diffusion-model" },
  { slot: "clip_l", flag: "--clip_l" },
  { slot: "clip_g", flag: "--clip_g" },
  { slot: "clip_vision", flag: "--clip_vision" },
  { slot: "t5xxl", flag: "--t5xxl" },
  { slot: "llm", flag: "--llm" },
  { slot: "vae", flag: "--vae" },
  { slot: "audio_vae", flag: "--audio-vae" },
  { slot: "taesd", flag: "--taesd" },
  { slot: "mmproj", flag: "--mmproj" },
  { slot: "vocoder", flag: "--model-vocoder" },
];

export const ALL_TASKS: GenerationTask[] = [
  "text_to_image",
  "image_to_image",
  "text_to_video",
  "image_to_video",
  "text_to_speech",
];

/** Samplers the engine accepts, in the order it reports them. */
export const SAMPLERS = [
  "euler",
  "euler_a",
  "heun",
  "dpm2",
  "dpm++2s_a",
  "dpm++2m",
  "dpm++2mv2",
  "ipndm",
  "ipndm_v",
  "lcm",
  "ddim_trailing",
  "tcd",
  "res_multistep",
  "res_2s",
  "er_sde",
  "euler_cfg_pp",
  "euler_a_cfg_pp",
  "euler_ge",
  "dpm++2m_sde",
  "dpm++2m_sde_bt",
  "lms",
];

/** Sigma schedules the engine accepts, in the order it reports them. */
export const SCHEDULERS = [
  "discrete",
  "normal",
  "karras",
  "exponential",
  "ays",
  "gits",
  "sgm_uniform",
  "simple",
  "smoothstep",
  "kl_optimal",
  "lcm",
  "bong_tangent",
  "ltx2",
  "logit_normal",
  "flux2",
  "flux",
  "beta",
];

/** The engine's built-in upscalers. Offered as suggestions rather than a
 *  closed list: a model dropped in the directory passed to
 *  `--hires-upscalers-dir` joins them under its own name, which is how an
 *  R-ESRGAN becomes selectable. */
export const UPSCALERS = [
  "Latent",
  "Latent (nearest)",
  "Latent (nearest-exact)",
  "Latent (antialiased)",
  "Latent (bicubic)",
  "Latent (bicubic antialiased)",
  "Lanczos",
  "Nearest",
  "None",
];

/** Canvas presets, matching the sizes these families are trained at. */
export const ASPECT_PRESETS: { id: string; width: number; height: number }[] = [
  { id: "portrait", width: 768, height: 1152 },
  { id: "landscape", width: 1152, height: 768 },
  { id: "square", width: 1024, height: 1024 },
];

/** A blank model, ready for the user to fill in. */
export function emptyModelSpec(): GenerationModelSpec {
  return {
    id: "",
    name: "",
    family: "",
    tasks: [],
    components: [],
    defaults: {
      width: 1024,
      height: 1024,
      steps: 20,
      cfgScale: 7,
      sampleMethod: "euler",
      flowShift: null,
      fps: 24,
      videoFrames: 33,
      frameGrid: "down_to4n_plus1",
    },
    minRamBytes: 0,
    license: {
      id: "",
      name: "",
      url: "",
      excludedTerritories: [],
      acceptanceRequired: false,
    },
    extraLaunchArgs: [],
  };
}
